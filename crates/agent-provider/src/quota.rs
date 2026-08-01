//! Subscription quota headers and stream events served by the Codex backend.

use std::collections::BTreeSet;

use agent_core::quota::{QuotaCredits, QuotaReachedKind, QuotaSnapshot, QuotaWindow};
use reqwest::header::HeaderMap;

/// Compatibility entry point for callers interested only in the default pool.
pub fn parse_quota_headers(headers: &HeaderMap) -> Option<QuotaSnapshot> {
    parse_pool(headers, "codex")
}

/// Parses every metered pool. The default `codex` pool is first, followed by
/// stable, normalized pool identifiers discovered in header names.
pub fn parse_all_quota_headers(headers: &HeaderMap) -> Vec<QuotaSnapshot> {
    let mut snapshots = Vec::new();
    if let Some(snapshot) = parse_quota_headers(headers) {
        snapshots.push(snapshot);
    }
    let mut ids = BTreeSet::new();
    for name in headers.keys() {
        if let Some(prefix) = name.as_str().strip_suffix("-primary-used-percent")
            && let Some(id) = prefix.strip_prefix("x-")
        {
            let id = normalize_id(id);
            if id != "codex" {
                ids.insert(id);
            }
        }
    }
    snapshots.extend(ids.into_iter().filter_map(|id| parse_pool(headers, &id)));
    snapshots
}

fn parse_pool(headers: &HeaderMap, id: &str) -> Option<QuotaSnapshot> {
    let header_id = id.replace('_', "-");
    let prefix = format!("x-{header_id}");
    let snapshot = QuotaSnapshot {
        limit_id: Some(normalize_id(id)),
        limit_name: header_str(headers, &format!("{prefix}-limit-name")).map(str::to_string),
        primary: parse_window(headers, &prefix, "primary"),
        secondary: parse_window(headers, &prefix, "secondary"),
        credits: parse_credits(headers),
        plan: None,
        reached: header_str(headers, "x-codex-rate-limit-reached-type")
            .and_then(|value| value.parse::<QuotaReachedKind>().ok()),
        promo: header_str(headers, "x-codex-promo-message").map(str::to_string),
    };
    (!snapshot.is_empty()).then_some(snapshot)
}

fn parse_window(headers: &HeaderMap, prefix: &str, family: &str) -> Option<QuotaWindow> {
    let used_percent = header_str(headers, &format!("{prefix}-{family}-used-percent"))?
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())?;
    let window = QuotaWindow {
        used_percent,
        window_minutes: header_str(headers, &format!("{prefix}-{family}-window-minutes"))
            .and_then(|value| value.parse::<i64>().ok()),
        resets_at_unix: header_str(headers, &format!("{prefix}-{family}-reset-at"))
            .and_then(|value| value.parse::<i64>().ok()),
    };
    (window.used_percent != 0.0
        || window.window_minutes.is_some_and(|minutes| minutes != 0)
        || window.resets_at_unix.is_some())
    .then_some(window)
}

fn parse_credits(headers: &HeaderMap) -> Option<QuotaCredits> {
    Some(QuotaCredits {
        has_credits: header_bool(headers, "x-codex-credits-has-credits")?,
        unlimited: header_bool(headers, "x-codex-credits-unlimited")?,
        balance: header_str(headers, "x-codex-credits-balance").map(str::to_string),
    })
}

fn header_bool(headers: &HeaderMap, name: &str) -> Option<bool> {
    match header_str(headers, name)? {
        value if value.eq_ignore_ascii_case("true") || value == "1" => Some(true),
        value if value.eq_ignore_ascii_case("false") || value == "0" => Some(false),
        _ => None,
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let value = headers.get(name)?.to_str().ok()?.trim();
    (!value.is_empty()).then_some(value)
}

fn normalize_id(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('-', "_")
}

/// Parses one streamed `codex.rate_limits` update. Stream events describe one
/// pool at a time; repeated events therefore remain separate quota updates.
pub fn parse_quota_event(value: &serde_json::Value) -> Option<QuotaSnapshot> {
    if value.get("type").and_then(serde_json::Value::as_str) != Some("codex.rate_limits") {
        return None;
    }
    let windows = value.get("rate_limits");
    let metered_id = value
        .get("metered_limit_name")
        .and_then(serde_json::Value::as_str)
        .map(normalize_id);
    let served_name = value
        .get("limit_name")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string);
    let snapshot = QuotaSnapshot {
        limit_id: Some(
            metered_id
                .or_else(|| served_name.as_deref().map(normalize_id))
                .unwrap_or_else(|| "codex".into()),
        ),
        limit_name: served_name,
        primary: windows.and_then(|window| event_window(window.get("primary"))),
        secondary: windows.and_then(|window| event_window(window.get("secondary"))),
        credits: event_credits(value.get("credits")),
        plan: value
            .get("plan_type")
            .or_else(|| value.get("plan"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|plan| !plan.is_empty())
            .map(str::to_string),
        reached: value
            .get("rate_limit_reached_type")
            .and_then(serde_json::Value::as_str)
            .and_then(|reached| reached.parse().ok()),
        promo: value
            .get("promo")
            .or_else(|| value.get("promo_message"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|promo| !promo.is_empty())
            .map(str::to_string),
    };
    (!snapshot.is_empty()).then_some(snapshot)
}

fn event_window(window: Option<&serde_json::Value>) -> Option<QuotaWindow> {
    let window = window?;
    let used_percent = window
        .get("used_percent")
        .and_then(serde_json::Value::as_f64)
        .filter(|value| value.is_finite())?;
    let parsed = QuotaWindow {
        used_percent,
        window_minutes: window
            .get("window_minutes")
            .and_then(serde_json::Value::as_i64),
        resets_at_unix: window.get("reset_at").and_then(serde_json::Value::as_i64),
    };
    Some(parsed)
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

pub fn quota_refusal_message(snapshot: Option<&QuotaSnapshot>) -> String {
    let cause = snapshot
        .and_then(|snapshot| snapshot.reached)
        .map_or("Subscription usage limit reached", QuotaReachedKind::label);
    let Some(window) = snapshot.and_then(QuotaSnapshot::most_consumed) else {
        return format!("{cause}.");
    };
    let scope = window
        .window_label()
        .map(|label| format!(" ({label})"))
        .unwrap_or_default();
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
    fn parses_all_named_pools_with_i64_windows_and_shared_account_state() {
        let snapshots = parse_all_quota_headers(&headers(&[
            ("x-codex-primary-used-percent", "42.5"),
            ("x-codex-primary-window-minutes", "3000000000"),
            ("x-codex-other-primary-used-percent", "7"),
            ("x-codex-other-primary-window-minutes", "10080"),
            ("x-codex-other-limit-name", "Other models"),
            ("x-codex-credits-has-credits", "true"),
            ("x-codex-credits-unlimited", "false"),
            ("x-codex-promo-message", "More capacity soon"),
        ]));
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].limit_id.as_deref(), Some("codex"));
        assert_eq!(
            snapshots[0].primary.unwrap().window_minutes,
            Some(3_000_000_000)
        );
        assert_eq!(snapshots[1].limit_id.as_deref(), Some("codex_other"));
        assert_eq!(snapshots[1].limit_name.as_deref(), Some("Other models"));
        assert!(snapshots[1].credits.is_some());
    }

    #[test]
    fn event_preserves_identity_plan_credits_promo_and_both_windows() {
        let snapshot = parse_quota_event(&serde_json::json!({
            "type": "codex.rate_limits",
            "metered_limit_name": "codex-other",
            "limit_name": "Other models",
            "plan_type": "pro",
            "promo": "promo",
            "rate_limit_reached_type": "rate_limit_reached",
            "rate_limits": {
                "primary": {"used_percent": 12.0, "window_minutes": 3000000000_i64, "reset_at": 1784989920_i64},
                "secondary": {"used_percent": 24.0, "window_minutes": 10080_i64, "reset_at": 1784990000_i64}
            },
            "credits": {"has_credits": true, "unlimited": false, "balance": "3.5"}
        }))
        .expect("rate limits event");
        assert_eq!(snapshot.limit_id.as_deref(), Some("codex_other"));
        assert_eq!(snapshot.limit_name.as_deref(), Some("Other models"));
        assert_eq!(snapshot.plan.as_deref(), Some("pro"));
        assert_eq!(snapshot.promo.as_deref(), Some("promo"));
        assert_eq!(snapshot.reached, Some(QuotaReachedKind::RateLimitReached));
        assert_eq!(
            snapshot.primary.unwrap().window_minutes,
            Some(3_000_000_000)
        );
        assert!(snapshot.credits.is_some());
    }

    #[test]
    fn malformed_or_absent_headers_yield_no_snapshot() {
        assert!(parse_quota_headers(&HeaderMap::new()).is_none());
        assert!(
            parse_quota_headers(&headers(&[(
                "x-codex-primary-used-percent",
                "not-a-number"
            )]))
            .is_none()
        );
        assert_eq!(
            parse_quota_headers(&headers(&[("x-codex-primary-used-percent", "180")]))
                .and_then(|snapshot| snapshot.primary)
                .map(|window| window.used_percent),
            Some(180.0)
        );
    }

    #[test]
    fn refusal_message_names_limit_and_reset() {
        let snapshot = QuotaSnapshot {
            primary: Some(QuotaWindow {
                used_percent: 100.0,
                window_minutes: Some(10_080),
                resets_at_unix: Some(1_784_989_920),
            }),
            reached: Some(QuotaReachedKind::RateLimitReached),
            ..QuotaSnapshot::default()
        };
        let message = quota_refusal_message(Some(&snapshot));
        assert!(message.contains("Rate limit reached"));
        assert!(message.contains("1-week window"));
        assert!(message.contains("resets at"));
    }
}
