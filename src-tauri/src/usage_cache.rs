//! Last-good usage readings, kept across runs.
//!
//! # Why this exists
//!
//! [`crate::switcher::read_snapshot`] degrades a failed usage fetch to
//! [`UsageStatus::Stale`], which is documented to mean "showing you the last
//! known values". There were no last known values: nothing was ever kept, so
//! the status meant "showing you nothing" and the whole screen read as broken.
//! Opening the app during a cold start, a network blip, or a rate-limit
//! backoff produced a wall of `··` with no explanation.
//!
//! # The trust model is the interesting part
//!
//! A cached reading cannot be served forever — quota moves whether or not this
//! app is watching — but *how long* it stays believable depends on why the
//! fetch failed, not just on its age:
//!
//! - **A 429 is a throttle, not news about the account.** The endpoint refused
//!   to talk to us; it said nothing about the quota, and the numbers we already
//!   have are as true as they were a moment ago. So the reading stays trusted
//!   until the window it describes actually rolls over.
//! - **Any other failure is no evidence at all.** A timeout means we do not
//!   know what happened, so the reading gets a much shorter horizon.
//!
//! Collapsing those two into one fixed expiry is what upstream tried first,
//! and their own notes record the result: a throttled account flipped to
//! unknown, became an ineligible switch target, and the fleet flapped.
//!
//! Past its horizon a reading is dropped rather than shown, because a stale
//! figure that looks live is worse than an honest absence — it can send the
//! switcher to an account that is actually spent.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::model::Usage;

const SCHEMA_VERSION: u32 = 1;

/// How long any reading may be served after a non-429 failure. A timeout tells
/// us nothing about whether the reading still holds.
const TRUST_MAX_AGE_S: i64 = 3_600;

/// Ceiling for a reading held through a rate-limit backoff, used when the
/// reading carries no reset of its own to expire against.
const RATE_LIMIT_TRUST_MAX_AGE_S: i64 = 7_200;

/// The error kind that means "throttled", as classified in `oauth.rs`.
pub const RATE_LIMITED: &str = "http-429";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    last_good: Usage,
    fetched_at: String,
    /// Kind of the most recent *failed* attempt, or `None` if the last attempt
    /// succeeded. Decides which trust horizon applies.
    last_error: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Persisted {
    schema_version: u32,
    /// Keyed by `Account::stable_key()` — never by email or slot, both of
    /// which change over an account's life.
    rows: HashMap<String, Row>,
}

#[derive(Debug)]
pub struct UsageCache {
    path: Option<PathBuf>,
    rows: HashMap<String, Row>,
}

impl UsageCache {
    /// Read the cache, or start empty. A malformed file is discarded rather
    /// than surfaced: the cost is one screen of honest unknowns, where
    /// refusing to start would be worse than the problem.
    pub fn load() -> Self {
        let path = crate::paths::usage_cache_path();
        let rows = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Persisted>(&bytes).ok())
            .filter(|p| p.schema_version == SCHEMA_VERSION)
            .map(|p| p.rows)
            .unwrap_or_default();
        Self {
            path: Some(path),
            rows,
        }
    }

    /// An in-memory cache that never touches disk. For tests.
    pub fn ephemeral() -> Self {
        Self {
            path: None,
            rows: HashMap::new(),
        }
    }

    /// A fresh reading. Replaces whatever was held and clears the error mark.
    pub fn record_success(&mut self, key: &str, usage: &Usage, at: DateTime<Utc>) {
        self.rows.insert(
            key.to_string(),
            Row {
                last_good: usage.clone(),
                fetched_at: at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                last_error: None,
            },
        );
    }

    /// A failed attempt. The held reading is untouched — only the reason it is
    /// being served changes, and with it how long it stays believable.
    pub fn record_failure(&mut self, key: &str, kind: &str) {
        if let Some(row) = self.rows.get_mut(key) {
            row.last_error = Some(kind.to_string());
        }
    }

    /// The last good reading and its age in seconds, when it is still worth
    /// believing. `None` once it is past its horizon, or was never recorded.
    pub fn serve(&self, key: &str, now: DateTime<Utc>) -> Option<(Usage, f64)> {
        let row = self.rows.get(key)?;
        let fetched_at = DateTime::parse_from_rfc3339(&row.fetched_at)
            .ok()?
            .with_timezone(&Utc);
        let age_s = now.signed_duration_since(fetched_at).num_seconds();
        // A clock that moved backwards leaves a future timestamp; treat that
        // as fresh rather than as an expired reading.
        if age_s < 0 {
            return Some((row.last_good.clone(), 0.0));
        }

        let throttled = row.last_error.as_deref() == Some(RATE_LIMITED);
        let horizon_s = if throttled {
            // The reading describes a window; it stops meaning anything when
            // that window rolls over, and not before.
            match earliest_future_reset(&row.last_good, now) {
                Some(seconds_until_reset) => seconds_until_reset.max(0) + age_s,
                None => RATE_LIMIT_TRUST_MAX_AGE_S,
            }
        } else {
            TRUST_MAX_AGE_S
        };

        (age_s < horizon_s).then(|| (row.last_good.clone(), age_s as f64))
    }

    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        let body = Persisted {
            schema_version: SCHEMA_VERSION,
            rows: self.rows.clone(),
        };
        let Ok(bytes) = serde_json::to_vec_pretty(&body) else { return };
        match crate::durable_fs::stage_sibling(path, &bytes, Some(0o600))
            .and_then(|staged| staged.commit())
        {
            Ok(()) => {}
            Err(e) => log::debug!("usage cache: could not persist ({e})"),
        }
    }
}

/// Seconds until the soonest window on this reading resets, if any is still
/// ahead. That instant is when the numbers stop describing anything.
fn earliest_future_reset(usage: &Usage, now: DateTime<Utc>) -> Option<i64> {
    let mut soonest: Option<i64> = None;
    let mut consider = |raw: &Option<String>| {
        let Some(text) = raw else { return };
        let Ok(parsed) = DateTime::parse_from_rfc3339(text) else {
            return;
        };
        let delta = parsed.with_timezone(&Utc).signed_duration_since(now).num_seconds();
        if delta > 0 {
            soonest = Some(soonest.map_or(delta, |s: i64| s.min(delta)));
        }
    };

    if let Some(w) = &usage.five_hour {
        consider(&w.resets_at);
    }
    if let Some(w) = &usage.seven_day {
        consider(&w.resets_at);
    }
    for w in usage.scoped.iter().flatten() {
        consider(&w.resets_at);
    }
    if let Some(s) = &usage.spend {
        consider(&s.resets_at);
    }
    soonest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UsageWindow;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-30T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn usage_with_reset(resets_at: Option<&str>) -> Usage {
        Usage {
            five_hour: Some(UsageWindow {
                pct: 42.0,
                resets_at: resets_at.map(str::to_string),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn nothing_is_served_before_anything_is_recorded() {
        assert!(UsageCache::ephemeral().serve("k", now()).is_none());
    }

    #[test]
    fn a_fresh_reading_comes_back_with_its_age() {
        let mut cache = UsageCache::ephemeral();
        cache.record_success("k", &usage_with_reset(None), now() - chrono::Duration::seconds(90));
        let (usage, age) = cache.serve("k", now()).expect("still fresh");
        assert_eq!(usage.five_hour.unwrap().pct, 42.0);
        assert!((age - 90.0).abs() < 1.0);
    }

    #[test]
    fn an_ordinary_failure_keeps_the_reading_for_an_hour_and_no_longer() {
        // A timeout is no evidence the reading still holds.
        let mut cache = UsageCache::ephemeral();
        cache.record_success("k", &usage_with_reset(None), now());
        cache.record_failure("k", "timeout");

        assert!(cache.serve("k", now() + chrono::Duration::seconds(TRUST_MAX_AGE_S - 60)).is_some());
        assert!(cache.serve("k", now() + chrono::Duration::seconds(TRUST_MAX_AGE_S + 60)).is_none());
    }

    #[test]
    fn a_throttle_keeps_the_reading_until_its_window_actually_resets() {
        // A 429 said nothing about the quota, so the figures are as true as
        // they were — until the window they describe rolls over.
        let mut cache = UsageCache::ephemeral();
        cache.record_success("k", &usage_with_reset(Some("2026-07-30T16:00:00Z")), now());
        cache.record_failure("k", RATE_LIMITED);

        // Three hours later, well past the ordinary hour, still trusted.
        assert!(cache.serve("k", now() + chrono::Duration::hours(3)).is_some());
        // Past the reset, the numbers describe a window that no longer exists.
        assert!(cache.serve("k", now() + chrono::Duration::hours(5)).is_none());
    }

    #[test]
    fn a_throttled_reading_with_no_reset_falls_back_to_a_ceiling() {
        let mut cache = UsageCache::ephemeral();
        cache.record_success("k", &usage_with_reset(None), now());
        cache.record_failure("k", RATE_LIMITED);

        assert!(cache
            .serve("k", now() + chrono::Duration::seconds(RATE_LIMIT_TRUST_MAX_AGE_S - 60))
            .is_some());
        assert!(cache
            .serve("k", now() + chrono::Duration::seconds(RATE_LIMIT_TRUST_MAX_AGE_S + 60))
            .is_none());
    }

    #[test]
    fn a_throttle_outlives_an_ordinary_failure_on_the_same_reading() {
        // The distinction upstream records: collapsing these into one expiry
        // made a throttled account an ineligible switch target and the fleet
        // flapped.
        let recorded = now();
        let mut throttled = UsageCache::ephemeral();
        throttled.record_success("k", &usage_with_reset(None), recorded);
        throttled.record_failure("k", RATE_LIMITED);

        let mut timed_out = UsageCache::ephemeral();
        timed_out.record_success("k", &usage_with_reset(None), recorded);
        timed_out.record_failure("k", "timeout");

        let later = recorded + chrono::Duration::seconds(TRUST_MAX_AGE_S + 600);
        assert!(throttled.serve("k", later).is_some());
        assert!(timed_out.serve("k", later).is_none());
    }

    #[test]
    fn a_later_success_clears_the_failure_mark() {
        let mut cache = UsageCache::ephemeral();
        cache.record_success("k", &usage_with_reset(None), now() - chrono::Duration::hours(3));
        cache.record_failure("k", RATE_LIMITED);
        // Recovered: the reading is fresh again and back on the short horizon.
        cache.record_success("k", &usage_with_reset(None), now());
        assert!(cache.serve("k", now() + chrono::Duration::seconds(TRUST_MAX_AGE_S + 60)).is_none());
    }

    #[test]
    fn a_failure_for_an_account_never_recorded_stores_nothing() {
        let mut cache = UsageCache::ephemeral();
        cache.record_failure("never-seen", RATE_LIMITED);
        assert!(cache.serve("never-seen", now()).is_none());
    }

    #[test]
    fn a_clock_that_moved_backwards_reads_as_fresh_rather_than_expired() {
        let mut cache = UsageCache::ephemeral();
        cache.record_success("k", &usage_with_reset(None), now() + chrono::Duration::hours(2));
        let (_, age) = cache.serve("k", now()).expect("a future stamp is not an expiry");
        assert_eq!(age, 0.0);
    }
}
