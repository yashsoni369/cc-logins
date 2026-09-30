//! What a last-known usage reading means *now*.
//!
//! In token-free mode the app reads usage only with a token Claude Code
//! itself keeps fresh, and never refreshes one. An account nobody has used
//! for a few hours therefore stops being measurable — but it also stops
//! spending quota, so its last reading stays true until a window rolls over.
//! This module turns (reading, when it was taken, now) into what the user
//! should see: the reading as-is, or with the windows that have since reset
//! shown at zero.
//!
//! Pure: no clock, no I/O. Callers pass `now`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::{SpendWindow, Usage, UsageWindow};

/// A reading younger than this is presented as live.
pub const LIVE_MAX_AGE_S: i64 = 15 * 60;

/// How current a displayed reading is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum UsageFreshness {
    /// Measured within [`LIVE_MAX_AGE_S`].
    Live,
    /// Older, but no window has reset since: the numbers still hold.
    #[serde(rename_all = "camelCase")]
    LastKnown { age_seconds: i64 },
    /// Older, and at least one window has reset since; those windows are
    /// shown at 0%.
    #[serde(rename_all = "camelCase")]
    Reset { age_seconds: i64 },
}

/// Project `usage`, taken at `fetched_at`, onto `now`.
pub fn project(
    usage: &Usage,
    fetched_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> (Usage, UsageFreshness) {
    // A clock that moved backwards leaves a future timestamp; treat it as
    // fresh rather than negative.
    let age_seconds = now.signed_duration_since(fetched_at).num_seconds().max(0);
    let mut projected = usage.clone();
    let mut any_reset = false;

    if let Some(window) = projected.five_hour.as_mut() {
        any_reset |= reset_window(window, now);
    }
    if let Some(window) = projected.seven_day.as_mut() {
        any_reset |= reset_window(window, now);
    }
    if let Some(windows) = projected.scoped.as_mut() {
        for window in windows.iter_mut() {
            any_reset |= reset_window(window, now);
        }
    }
    if let Some(spend) = projected.spend.as_mut() {
        any_reset |= reset_spend(spend, now);
    }

    let freshness = if age_seconds <= LIVE_MAX_AGE_S && !any_reset {
        UsageFreshness::Live
    } else if any_reset {
        UsageFreshness::Reset { age_seconds }
    } else {
        UsageFreshness::LastKnown { age_seconds }
    };
    (projected, freshness)
}

/// True when `resets_at` parses and is not after `now`.
fn has_reset(resets_at: Option<&str>, now: DateTime<Utc>) -> bool {
    resets_at
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .is_some_and(|at| at.with_timezone(&Utc) <= now)
}

/// Zero a window whose reset time has passed. The next window's reset time
/// is unknown until the account is measured again, so every derived field
/// goes with it rather than describing the old window.
fn reset_window(window: &mut UsageWindow, now: DateTime<Utc>) -> bool {
    if !has_reset(window.resets_at.as_deref(), now) {
        return false;
    }
    *window = UsageWindow {
        pct: 0.0,
        name: window.name.take(),
        ..UsageWindow::default()
    };
    true
}

fn reset_spend(spend: &mut SpendWindow, now: DateTime<Utc>) -> bool {
    if !has_reset(spend.resets_at.as_deref(), now) {
        return false;
    }
    spend.used = 0.0;
    spend.pct = 0.0;
    spend.severity = None;
    spend.resets_at = None;
    spend.countdown = None;
    spend.clock = None;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn window(pct: f64, resets_at: &str) -> UsageWindow {
        UsageWindow {
            pct,
            resets_at: Some(resets_at.to_string()),
            countdown: Some("2h".to_string()),
            expected_pct: Some(40.0),
            ..UsageWindow::default()
        }
    }

    fn usage() -> Usage {
        Usage {
            five_hour: Some(window(80.0, "2026-09-30T12:00:00Z")),
            seven_day: Some(window(55.0, "2026-10-03T00:00:00Z")),
            scoped: Some(vec![UsageWindow {
                name: Some("Fable".to_string()),
                ..window(30.0, "2026-09-30T11:00:00Z")
            }]),
            spend: None,
        }
    }

    #[test]
    fn fresh_reading_is_live_and_unchanged() {
        let fetched = at("2026-09-30T10:00:00Z");
        let (got, freshness) = project(&usage(), fetched, fetched + Duration::minutes(5));
        assert_eq!(freshness, UsageFreshness::Live);
        assert_eq!(got, usage());
    }

    #[test]
    fn old_reading_without_resets_is_last_known() {
        let fetched = at("2026-09-30T08:00:00Z");
        let now = at("2026-09-30T10:00:00Z");
        let (got, freshness) = project(&usage(), fetched, now);
        assert_eq!(
            freshness,
            UsageFreshness::LastKnown {
                age_seconds: 2 * 3600
            }
        );
        assert_eq!(got, usage());
    }

    #[test]
    fn windows_past_their_reset_show_zero_and_lose_derived_fields() {
        let fetched = at("2026-09-30T08:00:00Z");
        let now = at("2026-09-30T11:30:00Z");
        let (got, freshness) = project(&usage(), fetched, now);
        assert!(matches!(freshness, UsageFreshness::Reset { .. }));

        // The scoped window reset at 11:00: zeroed, name kept.
        let scoped = &got.scoped.as_ref().unwrap()[0];
        assert_eq!(scoped.pct, 0.0);
        assert_eq!(scoped.name.as_deref(), Some("Fable"));
        assert_eq!(scoped.resets_at, None);
        assert_eq!(scoped.expected_pct, None);

        // The five-hour window resets at 12:00, still ahead: untouched.
        assert_eq!(got.five_hour, usage().five_hour);
    }

    #[test]
    fn a_reset_makes_even_a_young_reading_non_live() {
        let fetched = at("2026-09-30T11:55:00Z");
        let now = at("2026-09-30T12:01:00Z");
        let (got, freshness) = project(&usage(), fetched, now);
        assert_eq!(freshness, UsageFreshness::Reset { age_seconds: 360 });
        assert_eq!(got.five_hour.unwrap().pct, 0.0);
    }

    #[test]
    fn spend_resets_too() {
        let spend = SpendWindow {
            used: 12.0,
            limit: 200.0,
            pct: 6.0,
            currency: "USD".to_string(),
            resets_at: Some("2026-10-01T00:00:00Z".to_string()),
            ..SpendWindow::default()
        };
        let reading = Usage {
            spend: Some(spend),
            ..Usage::default()
        };
        let (got, _) = project(
            &reading,
            at("2026-09-30T00:00:00Z"),
            at("2026-10-01T00:00:01Z"),
        );
        let spend = got.spend.unwrap();
        assert_eq!((spend.used, spend.pct), (0.0, 0.0));
        assert_eq!(spend.limit, 200.0);
    }

    #[test]
    fn future_fetch_time_counts_as_fresh() {
        let now = at("2026-09-30T10:00:00Z");
        let (_, freshness) = project(&usage(), now + Duration::minutes(3), now);
        assert_eq!(freshness, UsageFreshness::Live);
    }

    #[test]
    fn freshness_serialises_camel_case_with_a_kind_tag() {
        let json = serde_json::to_string(&UsageFreshness::LastKnown { age_seconds: 5 }).unwrap();
        assert_eq!(json, r#"{"kind":"lastKnown","ageSeconds":5}"#);
    }
}
