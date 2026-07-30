//! The usage endpoint's rolling-hour request budget, remembered across runs.
//!
//! # Why this exists
//!
//! `poller::poll_policy` already keeps the *sustained* rate under the measured
//! cap — see its doc comment for the numbers. Two things sat outside it.
//!
//! The budget lived entirely in memory, so every launch believed it had a
//! rested token. The endpoint's window is a trailing hour that capacity ages
//! out of, not a bucket that refills, so a process that restarts repeatedly
//! spends the whole hour on launches alone and then sits in a 429 loop it
//! cannot explain. That is a developer's inner loop, a crash loop, and — the
//! case that reaches users — one account polled from more than one machine.
//!
//! And the poller was not the only spender. `get_snapshot` and
//! `refresh_snapshot` fetch usage too, on their own cooldown, with no shared
//! accounting. A cadence that governs one of three request sources is not a
//! budget.
//!
//! # What it does not do
//!
//! Nothing here tries to widen the limit. The endpoint admits first-party
//! clients more generously, and identifying as one would mean sending Claude
//! Code's own User-Agent — deliberate circumvention of the server-side blocking
//! Anthropic deployed in 2026, and a risk borne by the user's account rather
//! than by us. This module makes the app live inside the limit it is given.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

const FILE_NAME: &str = "poll-budget.json";
const SCHEMA_VERSION: u32 = 1;

/// The trailing window capacity ages out of.
const WINDOW_S: i64 = 3_600;

/// Requests to spend per window before waiting for capacity.
///
/// Measured admission is ~28–30; this leaves several in hand so a manual
/// refresh, a switch, or a second machine on the same token still gets through
/// after the automatic cadence has taken its share.
const BUDGET: usize = 24;

/// Floor applied to any server-supplied `Retry-After`.
///
/// The endpoint is documented returning `retry-after: 0` alongside a 429 for
/// Max users. Honouring that literally means retrying immediately into a limit
/// that has not moved, which is how a client ends up in the permanent 429 loop
/// third-party status-line tools report.
const RETRY_AFTER_FLOOR_S: f64 = 360.0;

/// Never wait longer than this for capacity, whatever the arithmetic says.
const MAX_WAIT_S: f64 = 1_800.0;

/// Added past a server-supplied `Retry-After` before trying again.
///
/// Retrying at the exact instant the header names re-blocks: every client the
/// limit caught retries together and re-triggers it. The standard remedy is a
/// ~10% jitter or margin, and upstream measured the failure directly — 37 of
/// 39 blocks opened with an hour-long `Retry-After`, and 10 of those re-blocked
/// within 900s of a deadline-exact retry.
const RETRY_AFTER_MARGIN: f64 = 0.1;

/// A `Retry-After` longer than this is treated as pathological and clamped, so
/// a single header cannot silence the app beyond one window.
const RETRY_AFTER_CAP_S: f64 = 3_600.0;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Persisted {
    schema_version: u32,
    /// RFC-3339 instants, oldest first.
    requests: Vec<String>,
    rate_limited_at: Option<String>,
    /// When the honoured backoff lifts. Recency is measured from here.
    backoff_until: Option<String>,
    /// Interval the poller had backed off to, so a restart resumes it.
    interval_s: Option<f64>,
}

/// The ledger. Cheap to clone-free borrow; one instance per process.
#[derive(Debug)]
pub struct PollBudget {
    path: Option<PathBuf>,
    requests: VecDeque<DateTime<Utc>>,
    rate_limited_at: Option<DateTime<Utc>>,
    backoff_until: Option<DateTime<Utc>>,
    interval_s: Option<f64>,
}

impl PollBudget {
    /// Read the ledger, or start an empty one.
    ///
    /// An unreadable or malformed file is not an error worth surfacing: the
    /// cost of starting fresh is one over-eager launch, where refusing to run
    /// would be a rate limiter that bricks the app.
    pub fn load(data_dir: &Path) -> Self {
        let path = data_dir.join(FILE_NAME);
        let parsed = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Persisted>(&bytes).ok())
            .filter(|p| p.schema_version == SCHEMA_VERSION);

        let mut budget = Self {
            path: Some(path),
            requests: VecDeque::new(),
            rate_limited_at: None,
            backoff_until: None,
            interval_s: None,
        };

        if let Some(p) = parsed {
            budget.requests = p
                .requests
                .iter()
                .filter_map(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&Utc))
                .collect();
            budget.rate_limited_at = p
                .rate_limited_at
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc));
            budget.backoff_until = p
                .backoff_until
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Utc));
            budget.interval_s = p.interval_s.filter(|v| v.is_finite() && *v > 0.0);
        }
        budget.prune(Utc::now());
        budget
    }

    /// An in-memory ledger that never touches disk. For tests.
    pub fn ephemeral() -> Self {
        Self {
            path: None,
            requests: VecDeque::new(),
            rate_limited_at: None,
            backoff_until: None,
            interval_s: None,
        }
    }

    /// Count one request against the window, whoever made it.
    pub fn record_request(&mut self, at: DateTime<Utc>) {
        self.requests.push_back(at);
        self.prune(at);
        self.save();
    }

    /// Record a 429, and the interval to resume at after a restart.
    pub fn record_rate_limited(&mut self, at: DateTime<Utc>, retry_after_s: Option<f64>) {
        self.rate_limited_at = Some(at);
        // A server-supplied hint is used only when it asks for *more* patience
        // than our own floor — `retry-after: 0` is a documented response here —
        // and a margin is added on top so we are not one of the clients that
        // retries at the exact instant the limit lifts.
        let hinted = retry_after_s
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|v| (v * (1.0 + RETRY_AFTER_MARGIN)).min(RETRY_AFTER_CAP_S));
        let wait = hinted.map_or(RETRY_AFTER_FLOOR_S, |v| v.max(RETRY_AFTER_FLOOR_S));

        self.interval_s = Some(wait);
        self.backoff_until = Some(at + Duration::milliseconds((wait * 1000.0) as i64));
        self.save();
    }

    /// Whether the post-429 cadence floor should still apply.
    ///
    /// Anchored to when the backoff *lifts*, not to when the 429 arrived. With
    /// an hour-scale `Retry-After` those are a window apart, so measuring from
    /// the 429 lets it age out during the wait — the floor would expire before
    /// the first retry it exists to govern.
    pub fn recently_rate_limited(&self, now: DateTime<Utc>) -> bool {
        let anchor = self.backoff_until.or(self.rate_limited_at);
        anchor.is_some_and(|at| now.signed_duration_since(at).num_seconds() < WINDOW_S)
    }

    /// The interval a restart should resume at, if one was persisted while
    /// backed off. `None` once the 429 has aged out.
    pub fn resume_interval(&self, now: DateTime<Utc>) -> Option<f64> {
        self.recently_rate_limited(now).then_some(self.interval_s).flatten()
    }

    /// Seconds to wait before the next request is within budget. Zero when
    /// there is room.
    ///
    /// Capacity returns only as the oldest request ages out of the trailing
    /// hour, so the wait is measured from that instant rather than from any
    /// notion of a refill rate.
    pub fn wait_for_capacity(&self, now: DateTime<Utc>) -> f64 {
        let live = self.live_count(now);
        if live < BUDGET {
            return 0.0;
        }
        // The (live - BUDGET + 1)-th oldest is the one whose expiry frees a slot.
        let surplus = live - BUDGET;
        let Some(freeing) = self.live_iter(now).nth(surplus) else {
            return 0.0;
        };
        let expires_at = *freeing + Duration::seconds(WINDOW_S);
        let wait = expires_at.signed_duration_since(now).num_milliseconds() as f64 / 1000.0;
        wait.clamp(0.0, MAX_WAIT_S)
    }

    /// Requests still inside the window. Exposed for logging, not decisions.
    pub fn spent(&self, now: DateTime<Utc>) -> usize {
        self.live_count(now)
    }

    fn live_iter(&self, now: DateTime<Utc>) -> impl Iterator<Item = &DateTime<Utc>> {
        self.requests
            .iter()
            .filter(move |at| now.signed_duration_since(**at).num_seconds() < WINDOW_S)
    }

    fn live_count(&self, now: DateTime<Utc>) -> usize {
        self.live_iter(now).count()
    }

    fn prune(&mut self, now: DateTime<Utc>) {
        while let Some(front) = self.requests.front() {
            if now.signed_duration_since(*front).num_seconds() >= WINDOW_S {
                self.requests.pop_front();
            } else {
                break;
            }
        }
        // A clock that jumped backwards would otherwise let the ledger grow
        // without bound; nothing needs more entries than the window admits.
        while self.requests.len() > BUDGET * 4 {
            self.requests.pop_front();
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let body = Persisted {
            schema_version: SCHEMA_VERSION,
            requests: self
                .requests
                .iter()
                .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
                .collect(),
            rate_limited_at: self
                .rate_limited_at
                .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            backoff_until: self
                .backoff_until
                .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            interval_s: self.interval_s,
        };
        let Ok(bytes) = serde_json::to_vec_pretty(&body) else { return };
        // Staged and committed rather than written in place: a half-written
        // ledger that fails to parse silently resets the budget, which is the
        // exact failure this module exists to prevent.
        match crate::durable_fs::stage_sibling(path, &bytes, Some(0o600))
            .and_then(|staged| staged.commit())
        {
            Ok(()) => {}
            Err(e) => log::debug!("poll budget: could not persist ({e})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(offset_s: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-30T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + Duration::seconds(offset_s)
    }

    #[test]
    fn an_empty_ledger_admits_immediately() {
        let budget = PollBudget::ephemeral();
        assert_eq!(budget.wait_for_capacity(at(0)), 0.0);
    }

    #[test]
    fn spending_under_budget_still_admits() {
        let mut budget = PollBudget::ephemeral();
        for i in 0..(BUDGET - 1) {
            budget.record_request(at(i as i64));
        }
        assert_eq!(budget.wait_for_capacity(at(100)), 0.0);
    }

    #[test]
    fn a_spent_window_waits_for_the_oldest_request_to_age_out() {
        let mut budget = PollBudget::ephemeral();
        for i in 0..BUDGET {
            budget.record_request(at(i as i64));
        }
        // The oldest landed at t=0, so its slot frees at t=3600. Asked at
        // t=3100, that is 500s away — inside the cap, so the real figure shows.
        let wait = budget.wait_for_capacity(at(3_100));
        assert!((wait - 500.0).abs() < 1.0, "expected ~500s, got {wait}");
    }

    #[test]
    fn a_long_wait_is_capped_rather_than_leaving_the_app_silent_for_an_hour() {
        let mut budget = PollBudget::ephemeral();
        for i in 0..BUDGET {
            budget.record_request(at(i as i64));
        }
        // The arithmetic says ~3000s. Waiting that long means fifty minutes of
        // stale figures with no explanation, so the wait is capped and the
        // eventual 429 — handled by the backoff — is the better outcome.
        assert_eq!(budget.wait_for_capacity(at(600)), MAX_WAIT_S);
    }

    #[test]
    fn capacity_returns_as_requests_age_out_rather_than_on_a_refill() {
        let mut budget = PollBudget::ephemeral();
        for i in 0..BUDGET {
            budget.record_request(at(i as i64));
        }
        // One second past the oldest expiry, exactly one slot exists.
        assert_eq!(budget.wait_for_capacity(at(WINDOW_S + 1)), 0.0);
    }

    #[test]
    fn a_restart_storm_is_counted_the_same_as_any_other_request() {
        // 24 launches in two minutes is the developer inner loop that started
        // this; the ledger must not treat a fresh process as a rested token.
        let mut budget = PollBudget::ephemeral();
        for i in 0..BUDGET {
            budget.record_request(at(i as i64 * 5));
        }
        assert!(budget.wait_for_capacity(at(130)) > 0.0);
    }

    #[test]
    fn retry_after_zero_never_shortens_the_wait() {
        // The endpoint is documented returning `retry-after: 0` with a 429.
        let mut budget = PollBudget::ephemeral();
        budget.record_rate_limited(at(0), Some(0.0));
        assert_eq!(budget.resume_interval(at(1)), Some(RETRY_AFTER_FLOOR_S));
    }

    #[test]
    fn a_longer_retry_after_is_honoured_with_a_margin_past_the_deadline() {
        // Retrying at the exact instant the header names re-blocks: every
        // client the limit caught retries together. Upstream measured 10 of 39
        // blocks re-blocking within 900s of a deadline-exact retry.
        let mut budget = PollBudget::ephemeral();
        budget.record_rate_limited(at(0), Some(900.0));
        let resumed = budget.resume_interval(at(1)).expect("a backoff was recorded");
        assert!((resumed - 990.0).abs() < 0.001, "expected ~990s, got {resumed}");
    }

    #[test]
    fn a_pathological_retry_after_cannot_silence_the_app_past_one_window() {
        let mut budget = PollBudget::ephemeral();
        budget.record_rate_limited(at(0), Some(86_400.0));
        assert_eq!(budget.resume_interval(at(1)), Some(RETRY_AFTER_CAP_S));
    }

    #[test]
    fn the_post_429_floor_is_anchored_to_when_the_backoff_lifts() {
        // An hour-scale Retry-After is a whole window long. Measured from the
        // 429 itself, the floor expires *during* the wait — before the first
        // retry it exists to govern. Measured from the lift, it still applies.
        let mut budget = PollBudget::ephemeral();
        budget.record_rate_limited(at(0), Some(3_000.0));

        // 3300s of backoff. At t=3650 the 429 itself is over an hour old...
        assert!(at(3_650).signed_duration_since(at(0)).num_seconds() > WINDOW_S);
        // ...yet the floor still applies, because the backoff lifted at 3300.
        assert!(budget.recently_rate_limited(at(3_650)));
        // A window past the lift, it finally relaxes.
        assert!(!budget.recently_rate_limited(at(3_300 + WINDOW_S + 60)));
    }

    #[test]
    fn a_missing_retry_after_falls_back_to_the_floor() {
        let mut budget = PollBudget::ephemeral();
        budget.record_rate_limited(at(0), None);
        assert_eq!(budget.resume_interval(at(1)), Some(RETRY_AFTER_FLOOR_S));
    }

    #[test]
    fn the_rate_limit_signal_ages_out_a_window_after_the_backoff_lifts() {
        let mut budget = PollBudget::ephemeral();
        // 600s hinted -> 660s honoured, so the backoff lifts at t=660 and the
        // floor relaxes a window after *that*, not a window after the 429.
        budget.record_rate_limited(at(0), Some(600.0));
        assert!(budget.recently_rate_limited(at(WINDOW_S)));
        assert!(!budget.recently_rate_limited(at(660 + WINDOW_S + 60)));
        assert_eq!(budget.resume_interval(at(660 + WINDOW_S + 60)), None);
    }

    #[test]
    fn the_wait_is_bounded_however_the_arithmetic_lands() {
        let mut budget = PollBudget::ephemeral();
        for _ in 0..(BUDGET * 3) {
            budget.record_request(at(0));
        }
        assert!(budget.wait_for_capacity(at(0)) <= MAX_WAIT_S);
    }

    #[test]
    fn a_clock_moving_backwards_does_not_grow_the_ledger_without_bound() {
        let mut budget = PollBudget::ephemeral();
        for _ in 0..(BUDGET * 10) {
            budget.record_request(at(0));
        }
        assert!(budget.requests.len() <= BUDGET * 4);
    }
}
