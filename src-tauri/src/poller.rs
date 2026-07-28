//! Background usage poller and (opt-in) auto-switch daemon.
//!
//! Ported from **claude-swap** (MIT), `claude_swap/poll_policy.py` (the
//! adaptive cadence) and `claude_swap/autoswitch.py` (the decision core of
//! `AutoSwitchEngine.tick`) — <https://github.com/realiti4/claude-swap>. This
//! is a behavioural port of the decision policy, not the CLI plumbing: no
//! JSONL event stream, no `--once` cron mode, no quarantine/freshen
//! machinery (that needs per-account credential mutation this module never
//! performs), no threading — this crate is already inside a Tokio runtime
//! via `tauri`.
//!
//! # Architecture note: one fetch, not two
//!
//! Upstream decouples *usage collection* (a persistent per-account store,
//! independently scheduled per `poll_policy`) from the *decision tick*
//! (`settings.interval_seconds`, mostly reading that store). This crate has
//! no such store: [`crate::switcher::read_snapshot`] is the only reused
//! read path, and it fetches **every** account's usage on every call — there
//! is no way to fetch just the active account, or just one stale candidate.
//! So this port fuses collection and decision into one tick, and
//! [`poll_policy`] governs the sleep *between* whole-snapshot fetches rather
//! than a per-account schedule. `PollerConfig::interval_seconds` (mirroring
//! `AutoSwitchSettings.interval_seconds`) is honoured as a seed and as the
//! floor for the "nothing to do, don't busy-loop" cases — exactly upstream's
//! role for that field, which even there never feeds `poll_policy`'s own
//! constants. The safety property this whole module exists to protect —
//! never sustain a request rate the usage endpoint will 429 — is carried
//! entirely by [`poll_policy`]'s own floor ([`poll_policy::MIN_INTERVAL_S`]),
//! independent of whatever `interval_seconds` is configured to.
//!
//! # LegacyDecision core is pure
//!
//! [`decide`] takes a snapshot, a config, and a small piece of carried-over
//! state, and returns a [`LegacyDecision`] — no I/O, no clock reads (the caller
//! passes `now`), no mutation. [`run`] is the only impure part: it fetches,
//! records history, paints the tray, emits events, mutates [`LegacyDaemonState`]
//! between ticks, and — gated by [`PollerConfig::auto_switch_enabled`] —
//! calls [`crate::switcher::switch_to`].
//!
//! # Rules this module enforces (see [`decide`]'s doc comment for detail)
//!
//! 1. An account with unknown usage is never auto-skipped, and never wins a
//!    comparison by having its unknown usage treated as zero (inherited for
//!    free from [`crate::model::Account::headroom`] and
//!    [`crate::switcher::pick_target`], both already built that way).
//! 2. Cooldown: no switch within `cooldown_seconds` of the last one.
//! 3. Hysteresis: the account just switched away from is excluded as a
//!    target until its utilisation has dropped `hysteresis_pct` below
//!    `threshold`.
//! 4. All-exhausted returns [`LegacyDecision::Exhausted`] with the earliest known
//!    recovery time — never spins on a genuinely stuck state.
//! 5. `unhealthy_ticks` consecutive ticks of unreadable active usage before
//!    failover is considered.
//! 6. A decided switch is announced via [`LegacyDecision::Warn`] for
//!    `grace_seconds` before [`LegacyDecision::Switch`] is ever returned for it
//!    (`grace_seconds: 0` switches on the same tick it is decided).
//! 7. [`run`] calls [`crate::switcher::switch_to`] only when
//!    `config.auto_switch_enabled` is `true`, which
//!    [`PollerConfig::default`] sets to `false`.
//!
//! # Never touches WSL
//!
//! [`crate::switcher::read_snapshot`] builds only the `Native` environment
//! (see its doc comment) — it never calls [`crate::wsl`] at all. This module
//! calls nothing else that could, so a sleeping WSL distro is never touched
//! by construction, not by a runtime check this module has to remember to
//! make.
//!
//! # Never panics
//!
//! A panic inside a spawned Tokio task is silently swallowed by the runtime
//! unless something awaits its `JoinHandle` — which would kill this daemon
//! with nothing in the UI to explain why. So the network fetch runs inside
//! its own `tokio::spawn`, whose `JoinError` on panic is caught and logged
//! instead of propagating; every synchronous, potentially-panicking step in
//! the tick ([`decide`], tray rendering, history recording, the switch call
//! itself) is additionally wrapped in `std::panic::catch_unwind`. Nothing in
//! this file calls `.unwrap()`/`.expect()`/indexes without a bounds check on
//! data this process doesn't control (network responses, disk state).

use std::panic::AssertUnwindSafe;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tauri::image::Image;
use tauri::{AppHandle, Emitter, Manager};

use crate::history::HistoryStore;
use crate::model::{Account, Snapshot, UsageStatus};
use crate::switcher::{self, Strategy};
use crate::tray::{self, AmbientTheme, IconSpec, State as TrayState};

// ============================================================================
// 1. Adaptive poll cadence — ported from `claude_swap.poll_policy`.
// ============================================================================

/// Cadence policy for the `/api/oauth/usage` endpoint — every number in one
/// place, ported faithfully from `claude_swap/poll_policy.py`.
///
/// # Why this exists: the endpoint has a per-token budget
///
/// The endpoint enforces a per-access-token budget on non-first-party
/// clients: a **rolling ~60-minute window of ~28-30 requests per token**
/// (measured against the real endpoint, two runs: a rested token admitted 30
/// requests before the first HTTP 429; the post-drain 429 oscillation ended
/// exactly when the drain burst aged 60 minutes; steady 1-request/180s
/// polling then ran 96 minutes with zero 429s). It is **not** a bucket with
/// a refill rate — capacity returns only as old requests age out of the
/// trailing hour, so a burst saturates the token for up to a full hour, and
/// pausing does not restore headroom early. The exact edge algorithm is
/// undocumented and Anthropic can retune it, so the constants below lean
/// only on the robust parts: a sustained rate safely under the cap, and an
/// ~hour recovery horizon. Target: an **average of at most ~1
/// request/3 minutes per token** (20/hour vs. the ~28-30/hour cap), leaving
/// headroom for manual commands and the bounded urgent mode below.
///
/// Every account has its own token, so this budget applies per account, not
/// per machine — but [`crate::switcher::read_snapshot`] fetches every
/// account in one call (see the module doc comment), so in *this* port the
/// floor below effectively governs the tightest of every account's budgets
/// at once: one snapshot fetch spends one request against N tokens
/// simultaneously, so N never changes the sustainable cadence, only the
/// tightest single account does.
pub mod poll_policy {
    /// Normal cadence floor: movement can halve an interval down to this,
    /// never below. ~1 request/3 minutes per token, well under the ~28-30/
    /// hour saturation point.
    pub const MIN_INTERVAL_S: f64 = 180.0;

    /// The baseline cadence the daemon starts from.
    ///
    /// This used to be a user-facing setting ("Check usage at most every…").
    /// It is fixed here instead, because the range a user could safely pick
    /// from was never actually theirs to choose: anything below
    /// [`MIN_INTERVAL_S`] was silently overridden by this module, and anything
    /// above it was immediately overruled in the other direction by the
    /// adaptive logic below, which tightens on movement and backs off when
    /// nothing is happening. A control whose value is discarded at both ends
    /// is a lie about who is in charge.
    ///
    /// 300s: comfortably above the measured 180s floor rather than sitting on
    /// it, and the same cadence the Claude Code status-line tools use against
    /// this identical endpoint, so the number means the same thing here as it
    /// does elsewhere in the ecosystem.
    ///
    /// Users who want a fresher reading press Refresh, which is bounded
    /// separately by `commands::MANUAL_REFRESH_COOLDOWN`.
    pub const DEFAULT_INTERVAL_S: f64 = 300.0;

    /// Urgent mode: the active account, within [`ESCALATION_MARGIN_PCT`] of
    /// the switch threshold, with movement observed this tick (i.e.
    /// actually burning toward the limit). Bounded by construction: either
    /// the threshold is crossed (the engine switches away) or the movement
    /// stops (the next tick decays back to [`MIN_INTERVAL_S`]).
    pub const URGENT_INTERVAL_S: f64 = 60.0;

    /// Decay ceiling for an account whose usage is not moving.
    pub const ACTIVE_MAX_INTERVAL_S: f64 = 300.0;

    /// Default interval for a not-yet-measured account.
    pub const CANDIDATE_DEFAULT_INTERVAL_S: f64 = 300.0;

    /// A window whose binding pct moved at least this much between polls is
    /// being consumed somewhere (this machine, another PC, session mode) →
    /// tighten; an unmoved one backs off toward its ceiling.
    pub const MOVEMENT_DELTA_PCT: f64 = 1.0;

    /// ±fraction applied to each scheduled interval so independent poll
    /// loops (this daemon, a `cswap` CLI on the same machine, ...) drift
    /// apart instead of fetching in lockstep.
    pub const JITTER_FRAC: f64 = 0.1;

    /// AIMD floor for the first observed 429: probe no more often than this
    /// once any 429 has been seen recently, so capacity aging out of the
    /// trailing hour (up to ~30/hour) outpaces the probing.
    pub const POST_429_MIN_INTERVAL_S: f64 = 360.0;

    /// AIMD multiplicative-increase factor applied to the previous interval
    /// while 429s keep recurring on this token.
    pub const POST_429_BACKOFF_MULT: f64 = 1.5;

    /// AIMD ceiling — wider than the normal ceiling so a contended token
    /// (shared across several machines with no coordination between them)
    /// gets fair-shared by reaction alone.
    pub const POST_429_MAX_INTERVAL_S: f64 = 1800.0;

    /// The engine escalates to urgent cadence when the active account is
    /// within this margin of the switch threshold.
    pub const ESCALATION_MARGIN_PCT: f64 = 15.0;

    /// Never schedule a poll later than a known window reset (+ slack):
    /// stored usage is obsolete the moment the window rolls over.
    pub const RESET_SLACK_S: f64 = 60.0;

    /// Inputs to [`plan_after_fetch`] for one just-completed fetch.
    #[derive(Debug, Clone, Copy)]
    pub struct FetchOutcome {
        /// The interval this fetch was scheduled under, or `None` on the
        /// very first fetch (falls back to [`MIN_INTERVAL_S`] /
        /// [`CANDIDATE_DEFAULT_INTERVAL_S`]).
        pub prev_interval_s: Option<f64>,
        /// Binding utilisation (0..=100) as of the *previous* fetch, or
        /// `None` if unknown.
        pub prev_binding_pct: Option<f64>,
        /// Binding utilisation as of *this* fetch, or `None` if unknown.
        pub new_binding_pct: Option<f64>,
        /// Whether this fetch was for the account currently in use. This
        /// port only ever calls this with `true` — see the module doc
        /// comment for why there is no separate "candidate" schedule here.
        pub is_active: bool,
        /// The configured switch threshold (0..=100), for urgent-mode
        /// escalation.
        pub threshold: f64,
        /// Whether any account's fetch showed evidence of a 429 within
        /// [`RECENT_429_WINDOW_S`] — for the exact 3600s window this
        /// constant names, callers track "was there a 429 recently" over
        /// time themselves and pass the boolean in.
        pub recent_429: bool,
    }

    /// `(next_poll_at, interval_s)` for a just-completed fetch. `now`,
    /// `jitter_sample` (expected uniform in `[0, 1)`), and the two resolved
    /// reset timestamps (already computed by the caller from whatever usage
    /// windows it has) are all explicit parameters rather than read from a
    /// clock or an RNG, so this stays a pure, exhaustively testable
    /// function — mirrors `plan_after_fetch` in the Python source.
    ///
    /// Movement (binding pct changed ≥ [`MOVEMENT_DELTA_PCT`] since the
    /// previous fetch) halves the interval, floored at [`MIN_INTERVAL_S`] —
    /// or drops to [`URGENT_INTERVAL_S`] when active and moving inside the
    /// escalation band. No movement backs off ×1.5 toward the ceiling;
    /// unknown utilisation uses the default. A recent 429 floors the
    /// cadence at [`POST_429_MIN_INTERVAL_S`] and grows it multiplicatively
    /// toward [`POST_429_MAX_INTERVAL_S`] (AIMD). The scheduled time gets
    /// [`JITTER_FRAC`] noise, is never later than the next known window
    /// reset (+ [`RESET_SLACK_S`]), and a known-at-limit account skips
    /// straight to the reset that frees it (the learned interval is kept
    /// for its return).
    pub fn plan_after_fetch(
        outcome: &FetchOutcome,
        now: f64,
        jitter_sample: f64,
        earliest_future_reset_ts: Option<f64>,
        limiting_reset_ts: Option<f64>,
    ) -> (f64, f64) {
        let default = if outcome.is_active {
            MIN_INTERVAL_S
        } else {
            CANDIDATE_DEFAULT_INTERVAL_S
        };
        let ceiling = ACTIVE_MAX_INTERVAL_S;
        let base = outcome.prev_interval_s.unwrap_or(default);

        let (moving, mut interval) = match (outcome.prev_binding_pct, outcome.new_binding_pct) {
            (Some(prev), Some(new)) if (new - prev).abs() >= MOVEMENT_DELTA_PCT => {
                (true, (base / 2.0).max(MIN_INTERVAL_S))
            }
            (Some(_), Some(_)) => (false, (base * 1.5).max(MIN_INTERVAL_S).min(ceiling)),
            _ => (false, default),
        };

        if outcome.is_active
            && moving
            && !outcome.recent_429
            && matches!(outcome.new_binding_pct, Some(p) if p >= outcome.threshold - ESCALATION_MARGIN_PCT)
        {
            interval = URGENT_INTERVAL_S;
        }

        if outcome.recent_429 {
            let increased = (base * POST_429_BACKOFF_MULT).max(POST_429_MIN_INTERVAL_S);
            interval = interval.max(increased).min(POST_429_MAX_INTERVAL_S);
        }

        let jitter = 1.0 + JITTER_FRAC * (2.0 * jitter_sample - 1.0);
        let mut next_poll = now + interval * jitter;

        let at_limit_known = matches!(outcome.new_binding_pct, Some(p) if p >= 100.0);
        if at_limit_known {
            if let Some(reset_ts) = limiting_reset_ts {
                if reset_ts > next_poll {
                    next_poll = reset_ts;
                }
            }
        } else if let Some(reset_ts) = earliest_future_reset_ts {
            next_poll = next_poll.min(reset_ts + RESET_SLACK_S);
        }

        (next_poll, interval)
    }
}

// ============================================================================
// 2. Configuration.
// ============================================================================

/// Poller settings, deliberately named and defaulted to agree with
/// `claude_swap.settings.AutoSwitchSettings` so a machine running both this
/// app and the `cswap` CLI sees the same policy from either.
#[derive(Debug, Clone)]
pub struct PollerConfig {
    /// Switch when the active account's binding utilisation reaches this
    /// percentage (0..=100).
    pub threshold: f64,
    /// Baseline tick interval in seconds. See the module doc comment for why
    /// this does *not* drive the usage-fetch cadence directly — that is
    /// [`poll_policy`]'s job — but is honoured as the seed interval and as
    /// the floor for "nothing to do" backoff, exactly its role upstream.
    pub interval_seconds: f64,
    /// Minimum time between two switches.
    pub cooldown_seconds: f64,
    /// How far below `threshold` the account just switched away from must
    /// drop before it is eligible to be switched back to.
    pub hysteresis_pct: f64,
    /// Target-selection strategy — see [`crate::switcher::Strategy`].
    pub strategy: Strategy,
    /// Consecutive ticks of unreadable active-account usage before failover
    /// is considered. Absorbs a single blip.
    pub unhealthy_ticks: u32,
    /// Seconds a decided switch is announced via [`LegacyDecision::Warn`] before
    /// it is carried out. `0` switches on the same tick it is decided.
    pub grace_seconds: f64,
    /// Whether [`run`] is allowed to actually call
    /// [`crate::switcher::switch_to`]. **Must default to `false`** — see the
    /// module doc comment, rule 7.
    pub auto_switch_enabled: bool,
}

impl Default for PollerConfig {
    fn default() -> Self {
        Self {
            threshold: 90.0,
            interval_seconds: poll_policy::DEFAULT_INTERVAL_S,
            cooldown_seconds: 300.0,
            hysteresis_pct: 10.0,
            strategy: Strategy::MostHeadroom,
            unhealthy_ticks: 3,
            grace_seconds: 60.0,
            // A tool that starts moving credentials before being asked is
            // not trustworthy. This is the load-bearing default: nothing in
            // this module may ever construct a `PollerConfig` with this
            // flag flipped on implicitly.
            auto_switch_enabled: false,
        }
    }
}

// ============================================================================
// 3. Carried-over state and the pure decision core.
// ============================================================================

/// State carried from one [`decide`] call to the next by [`run`]. Everything
/// here is plain data — no I/O, no interior mutability — so tests can
/// construct any point in the state machine directly without simulating the
/// ticks that would normally produce it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LegacyDaemonState {
    /// Epoch seconds of the last completed switch, or `None` if none yet
    /// this run.
    pub last_switch_at: Option<f64>,
    /// The account most recently switched *away* from — the hysteresis
    /// target (rule 3).
    pub last_switch_from: Option<u32>,
    /// Consecutive ticks (through and including the one about to be
    /// decided) where the active account's usage has been unreadable.
    /// [`run`] updates this via [`next_unhealthy_ticks`] before calling
    /// [`decide`] each tick.
    pub unhealthy_ticks: u32,
    /// A switch decided but not yet carried out — the in-progress grace
    /// countdown (rule 6).
    pub pending: Option<LegacyPendingSwitch>,
}

/// A switch [`decide`] has committed to but is still counting down before
/// performing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LegacyPendingSwitch {
    pub from: u32,
    pub to: u32,
    /// Epoch seconds when this countdown started.
    pub decided_at: f64,
}

/// What [`decide`] recommends this tick. Carries no side effects of its own
/// — [`run`] is what turns a `Switch` into an actual call to
/// [`crate::switcher::switch_to`], gated by
/// [`PollerConfig::auto_switch_enabled`].
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum LegacyDecision {
    /// Nothing to do this tick.
    Hold,
    /// A switch to `account` has been decided and is counting down;
    /// `seconds_left` is how much of `grace_seconds` remains.
    Warn { account: u32, seconds_left: f64 },
    /// Perform the switch now (grace has elapsed, or `grace_seconds == 0`).
    Switch { from: u32, to: u32 },
    /// Every account that could plausibly be used is known to be at its
    /// limit. `earliest_reset` is the RFC 3339 timestamp of the soonest
    /// moment any of them recovers, when provable.
    Exhausted { earliest_reset: Option<String> },
}

/// Advance the unhealthy-tick counter for *this* tick's observation.
/// Resets to 0 the moment usage is readable again — a single good reading
/// clears the streak, matching upstream's `_unhealthy_ticks = 0` on any
/// known headroom.
pub fn next_unhealthy_ticks(previous: u32, active_usage_known_this_tick: bool) -> u32 {
    if active_usage_known_this_tick {
        0
    } else {
        previous.saturating_add(1)
    }
}

/// Rule 3 (hysteresis), scoped to exactly the account named by
/// `state.last_switch_from`. Every other account — including one with
/// unknown usage — is unaffected: rule 1 (unknown is never auto-skipped) is
/// about candidate selection in general, and this is a narrower, different
/// exclusion (anti-flap on the one account just vacated), not a
/// reinterpretation of "unknown" as disqualifying.
fn hysteresis_ok(account: &Account, state: &LegacyDaemonState, config: &PollerConfig) -> bool {
    if state.last_switch_from != Some(account.number) {
        return true;
    }
    let floor = config.threshold - config.hysteresis_pct;
    matches!(account.headroom(), Some(h) if (100.0 - h) <= floor)
}

/// `(pct, resets_at)` for every window on `account` — five-hour, seven-day,
/// and scoped — mirroring `oauth::relevant_windows` closely enough for the
/// exhaustion/recovery math below, without the model-name filter (this port
/// has no per-model configuration; see [`crate::model::Account::headroom`],
/// which already folds every window in unconditionally).
fn account_reset_windows(account: &Account) -> Vec<(f64, Option<&str>)> {
    let mut out = Vec::new();
    if let Some(usage) = &account.usage {
        if let Some(w) = &usage.five_hour {
            out.push((w.pct, w.resets_at.as_deref()));
        }
        if let Some(w) = &usage.seven_day {
            out.push((w.pct, w.resets_at.as_deref()));
        }
        for w in usage.scoped.iter().flatten() {
            out.push((w.pct, w.resets_at.as_deref()));
        }
    }
    out
}

fn parse_rfc3339_epoch(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp())
}

/// Epoch when `account` becomes usable again — the *latest* reset among its
/// `>= 100%` windows (it isn't usable until every blocking window clears) —
/// or `None` when the account isn't blocked at all, or when it is blocked by
/// a window with no (or unparseable) reset time, which makes the recovery
/// instant unprovable. Mirrors `AutoSwitchEngine._earliest_recovery`'s
/// per-account step.
fn account_recovery_ts(account: &Account) -> Option<i64> {
    let mut latest: Option<i64> = None;
    let mut any_blocking = false;
    for (pct, resets_at) in account_reset_windows(account) {
        if pct >= 100.0 {
            any_blocking = true;
            let ts = resets_at.and_then(parse_rfc3339_epoch)?;
            latest = Some(latest.map_or(ts, |l| l.max(ts)));
        }
    }
    if !any_blocking {
        return None;
    }
    latest
}

/// The earliest moment any of `accounts` becomes usable again, or `None`
/// when that can't be proven (any one of them blocked with an unknown reset
/// makes the whole answer unprovable — see [`account_recovery_ts`]).
fn earliest_recovery_ts(accounts: &[&Account]) -> Option<i64> {
    let mut earliest: Option<i64> = None;
    for account in accounts {
        let ts = account_recovery_ts(account)?;
        earliest = Some(earliest.map_or(ts, |e| e.min(ts)));
    }
    earliest
}

/// The pure decision core. See the module doc comment for the full rule
/// list; in short:
///
/// - No active account → [`LegacyDecision::Hold`] (nothing to warn about or
///   switch away from).
/// - The active account's binding utilisation below `threshold` (and
///   readable) → [`LegacyDecision::Hold`].
/// - Unreadable active usage only triggers failover after
///   `config.unhealthy_ticks` consecutive ticks (`state.unhealthy_ticks`,
///   which [`run`] updates via [`next_unhealthy_ticks`] *before* calling
///   this) — one blip never ejects the active account.
/// - A switch is needed but the last one was within `cooldown_seconds` →
///   [`LegacyDecision::Hold`] (rule 2).
/// - Candidates are ranked by `config.strategy` via
///   [`crate::switcher::pick_target`] (which already never treats unknown
///   usage as zero — rule 1), after excluding the hysteresis-blocked
///   account (rule 3, [`hysteresis_ok`]).
/// - A candidate found: counts down via [`LegacyDecision::Warn`] for
///   `grace_seconds`, then [`LegacyDecision::Switch`] (rule 6). `state.pending`
///   carries the countdown's start time across ticks; a different target
///   winning mid-countdown restarts the countdown for the new target rather
///   than inheriting the old elapsed time.
/// - No candidate found: [`LegacyDecision::Exhausted`] only when every account
///   that could plausibly be used (the active one, plus every account
///   [`crate::model::Account::is_switchable`] would allow) is *known* to be
///   at its limit — an unknown reading anywhere means "not yet", never
///   "give up" (rule 4). Otherwise [`LegacyDecision::Hold`] — blocked this tick,
///   but not proven stuck.
///
/// `now` is epoch seconds, passed in rather than read from a clock so this
/// function is fully deterministic and needs no fixture beyond its
/// arguments.
pub fn decide(
    snapshot: &Snapshot,
    config: &PollerConfig,
    state: &LegacyDaemonState,
    now: f64,
) -> LegacyDecision {
    let Some(active) = snapshot.active_account() else {
        return LegacyDecision::Hold;
    };
    let active_number = active.number;
    let active_headroom = active.headroom();

    let need_switch = match active_headroom {
        Some(h) => 100.0 - h >= config.threshold,
        None => state.unhealthy_ticks >= config.unhealthy_ticks,
    };
    if !need_switch {
        return LegacyDecision::Hold;
    }

    // Rule 2: cooldown. Applied uniformly (no at-limit bypass) — a
    // deliberate simplification versus upstream's trigger-dependent
    // exception, kept because it makes the rule's tests unambiguous and
    // because a strict floor still self-heals within `cooldown_seconds`.
    if let Some(last) = state.last_switch_at {
        if now - last < config.cooldown_seconds {
            return LegacyDecision::Hold;
        }
    }

    let all_accounts: Vec<Account> = snapshot
        .environments
        .iter()
        .flat_map(|e| e.accounts.iter().cloned())
        .collect();

    // Rule 3: hysteresis pre-filter, scoped to `last_switch_from` only.
    let candidates: Vec<Account> = all_accounts
        .iter()
        .filter(|a| hysteresis_ok(a, state, config))
        .cloned()
        .collect();

    // Rule 1 lives in `pick_target`/`Account::headroom`: unknown usage is
    // never treated as zero and never auto-skipped.
    if let Some(target) = switcher::pick_target(&candidates, config.strategy) {
        let to = target.number;

        if config.grace_seconds <= 0.0 {
            return LegacyDecision::Switch {
                from: active_number,
                to,
            };
        }

        return match &state.pending {
            Some(p) if p.to == to => {
                let elapsed = (now - p.decided_at).max(0.0);
                if elapsed >= config.grace_seconds {
                    LegacyDecision::Switch {
                        from: p.from,
                        to: p.to,
                    }
                } else {
                    LegacyDecision::Warn {
                        account: to,
                        seconds_left: config.grace_seconds - elapsed,
                    }
                }
            }
            // No countdown in progress, or it was counting down for a
            // different target — (re)start the countdown fresh.
            _ => LegacyDecision::Warn {
                account: to,
                seconds_left: config.grace_seconds,
            },
        };
    }

    // Rule 4: no viable candidate. Only declare Exhausted when every
    // account that could plausibly be used — the active one, plus every
    // manually switchable account, hysteresis exclusion NOT applied
    // here (a healthy-but-hysteresis-blocked account means we are merely
    // blocked this tick, not stuck) — is *known* to be at its limit.
    let relevant: Vec<&Account> = all_accounts
        .iter()
        .filter(|a| a.active || a.is_switchable())
        .collect();
    let any_unknown = relevant.iter().any(|a| a.headroom().is_none());
    let all_at_limit = !relevant.is_empty()
        && !any_unknown
        && relevant
            .iter()
            .all(|a| matches!(a.headroom(), Some(h) if h <= 0.0));

    if all_at_limit {
        let earliest_reset = earliest_recovery_ts(&relevant)
            .and_then(|ts| DateTime::<Utc>::from_timestamp(ts, 0))
            .map(|dt| dt.to_rfc3339());
        LegacyDecision::Exhausted { earliest_reset }
    } else {
        LegacyDecision::Hold
    }
}

/// An automatic switch committed under one immutable policy revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingSwitch {
    pub from: u32,
    pub to: u32,
    pub deadline: DateTime<Utc>,
    pub policy_revision: u64,
}

/// Revision-aware daemon state. Wall-clock instants are absolute so a grace
/// deadline can wake independently from the network polling cadence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonState {
    pub last_switch_at: Option<DateTime<Utc>>,
    pub last_switch_from: Option<u32>,
    pub unhealthy_ticks: u32,
    pub pending: Option<PendingSwitch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "kind"
)]
pub enum Decision {
    Disabled,
    Paused {
        until: DateTime<Utc>,
    },
    Monitoring,
    Cooldown {
        until: DateTime<Utc>,
    },
    Warning {
        from: u32,
        to: u32,
        deadline: DateTime<Utc>,
    },
    Switch {
        from: u32,
        to: u32,
        policy_revision: u64,
    },
    Exhausted {
        earliest_reset: Option<DateTime<Utc>>,
    },
    Degraded {
        reason: crate::runtime::DegradedReason,
    },
}

impl Decision {
    pub fn phase(&self) -> crate::runtime::DaemonPhase {
        match self {
            Self::Disabled => crate::runtime::DaemonPhase::Disabled,
            Self::Paused { until } => crate::runtime::DaemonPhase::Paused { until: *until },
            Self::Monitoring => crate::runtime::DaemonPhase::Monitoring,
            Self::Cooldown { until } => crate::runtime::DaemonPhase::Cooldown { until: *until },
            Self::Warning { from, to, deadline } => crate::runtime::DaemonPhase::Warning {
                from: *from,
                to: *to,
                deadline: *deadline,
            },
            Self::Switch { from, to, .. } => crate::runtime::DaemonPhase::Switching {
                from: *from,
                to: *to,
            },
            Self::Exhausted { earliest_reset } => crate::runtime::DaemonPhase::Exhausted {
                earliest_reset: *earliest_reset,
            },
            Self::Degraded { reason } => crate::runtime::DaemonPhase::Degraded { reason: *reason },
        }
    }
}

/// Complete state owned by the interruptible poller loop.
#[derive(Debug, Clone)]
pub struct PollerLoopState {
    pub policy: crate::runtime::RuntimePolicy,
    pub daemon: DaemonState,
    pub last_trusted_snapshot: Option<Snapshot>,
    pub requires_fresh_snapshot: bool,
}

impl PollerLoopState {
    pub fn new(policy: crate::runtime::RuntimePolicy) -> Self {
        Self {
            policy,
            daemon: DaemonState::default(),
            last_trusted_snapshot: None,
            requires_fresh_snapshot: true,
        }
    }

    /// Apply only a newer complete policy. Every accepted revision cancels a
    /// decision produced under the old one and requires one fresh snapshot
    /// before automatic switching can resume.
    pub fn apply_policy(
        &mut self,
        policy: crate::runtime::RuntimePolicy,
        _now: DateTime<Utc>,
    ) -> bool {
        if policy.revision <= self.policy.revision {
            return false;
        }
        self.policy = policy;
        self.daemon.pending = None;
        self.requires_fresh_snapshot = true;
        true
    }

    pub fn decision_at(&self, now: DateTime<Utc>) -> Decision {
        if !self.policy.auto_switch_enabled {
            return Decision::Disabled;
        }
        if let Some(until) = self.policy.paused_until.filter(|until| *until > now) {
            return Decision::Paused { until };
        }
        if self.requires_fresh_snapshot {
            return Decision::Monitoring;
        }
        match self.daemon.pending {
            Some(pending) if pending.policy_revision == self.policy.revision => {
                if now >= pending.deadline {
                    Decision::Switch {
                        from: pending.from,
                        to: pending.to,
                        policy_revision: pending.policy_revision,
                    }
                } else {
                    Decision::Warning {
                        from: pending.from,
                        to: pending.to,
                        deadline: pending.deadline,
                    }
                }
            }
            _ => Decision::Monitoring,
        }
    }

    pub fn on_fetch_failed(&self, now: DateTime<Utc>) -> Decision {
        match self.policy_gate(now) {
            Some(decision) => decision,
            None => Decision::Degraded {
                reason: crate::runtime::DegradedReason::FetchFailed,
            },
        }
    }

    pub fn on_snapshot(&mut self, snapshot: Snapshot, now: DateTime<Utc>) -> Decision {
        self.last_trusted_snapshot = Some(snapshot.clone());
        self.requires_fresh_snapshot = false;

        if let Some(decision) = self.policy_gate(now) {
            self.daemon.pending = None;
            return decision;
        }

        let active_known = snapshot
            .active_account()
            .and_then(Account::headroom)
            .is_some();
        self.daemon.unhealthy_ticks =
            next_unhealthy_ticks(self.daemon.unhealthy_ticks, active_known);

        let Some(active) = snapshot.active_account() else {
            self.daemon.pending = None;
            return Decision::Monitoring;
        };
        let needs_switch = match active.headroom() {
            Some(headroom) => 100.0 - headroom >= self.policy.threshold,
            None => self.daemon.unhealthy_ticks >= self.policy.unhealthy_ticks,
        };
        if !needs_switch {
            self.daemon.pending = None;
            return if active_known {
                Decision::Monitoring
            } else {
                Decision::Degraded {
                    reason: crate::runtime::DegradedReason::UsageUnknown,
                }
            };
        }

        if let Some(last_switch_at) = self.daemon.last_switch_at {
            let until = add_std_duration(last_switch_at, self.policy.cooldown);
            if now < until {
                self.daemon.pending = None;
                return Decision::Cooldown { until };
            }
        }

        let legacy_config = PollerConfig {
            threshold: self.policy.threshold,
            interval_seconds: poll_policy::DEFAULT_INTERVAL_S,
            cooldown_seconds: 0.0,
            hysteresis_pct: self.policy.hysteresis_pct,
            strategy: self.policy.strategy,
            unhealthy_ticks: self.policy.unhealthy_ticks,
            grace_seconds: 0.0,
            auto_switch_enabled: self.policy.auto_switch_enabled,
        };
        let legacy_state = LegacyDaemonState {
            last_switch_at: None,
            last_switch_from: self.daemon.last_switch_from,
            unhealthy_ticks: self.daemon.unhealthy_ticks,
            pending: None,
        };
        match decide(
            &snapshot,
            &legacy_config,
            &legacy_state,
            datetime_epoch_seconds(now),
        ) {
            LegacyDecision::Switch { from, to } => self.commit_candidate(from, to, now),
            LegacyDecision::Exhausted { earliest_reset } => {
                self.daemon.pending = None;
                Decision::Exhausted {
                    earliest_reset: earliest_reset
                        .as_deref()
                        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                        .map(|value| value.with_timezone(&Utc)),
                }
            }
            LegacyDecision::Hold => {
                self.daemon.pending = None;
                let has_unknown = snapshot
                    .environments
                    .iter()
                    .flat_map(|environment| environment.accounts.iter())
                    .filter(|account| account.active || account.is_switchable())
                    .any(|account| account.headroom().is_none());
                if has_unknown {
                    Decision::Degraded {
                        reason: crate::runtime::DegradedReason::UsageUnknown,
                    }
                } else {
                    Decision::Monitoring
                }
            }
            // Grace is forced to zero above, so the legacy selector can never
            // produce a relative countdown.
            LegacyDecision::Warn { .. } => Decision::Monitoring,
        }
    }

    pub fn complete_switch(&mut self, from: u32, at: DateTime<Utc>) {
        self.daemon.last_switch_at = Some(at);
        self.daemon.last_switch_from = Some(from);
        self.daemon.pending = None;
        self.requires_fresh_snapshot = true;
    }

    fn policy_gate(&self, now: DateTime<Utc>) -> Option<Decision> {
        if !self.policy.auto_switch_enabled {
            Some(Decision::Disabled)
        } else {
            self.policy
                .paused_until
                .filter(|until| *until > now)
                .map(|until| Decision::Paused { until })
        }
    }

    fn commit_candidate(&mut self, from: u32, to: u32, now: DateTime<Utc>) -> Decision {
        if self.policy.grace.is_zero() {
            self.daemon.pending = None;
            return Decision::Switch {
                from,
                to,
                policy_revision: self.policy.revision,
            };
        }

        let pending = match self.daemon.pending {
            Some(pending)
                if pending.from == from
                    && pending.to == to
                    && pending.policy_revision == self.policy.revision =>
            {
                pending
            }
            _ => PendingSwitch {
                from,
                to,
                deadline: add_std_duration(now, self.policy.grace),
                policy_revision: self.policy.revision,
            },
        };
        self.daemon.pending = Some(pending);
        if now >= pending.deadline {
            Decision::Switch {
                from,
                to,
                policy_revision: pending.policy_revision,
            }
        } else {
            Decision::Warning {
                from,
                to,
                deadline: pending.deadline,
            }
        }
    }
}

fn add_std_duration(at: DateTime<Utc>, duration: Duration) -> DateTime<Utc> {
    chrono::Duration::from_std(duration)
        .ok()
        .and_then(|offset| at.checked_add_signed(offset))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

fn datetime_epoch_seconds(at: DateTime<Utc>) -> f64 {
    at.timestamp() as f64 + f64::from(at.timestamp_subsec_nanos()) / 1_000_000_000.0
}

// ============================================================================
// 4. The supervised loop.
// ============================================================================

#[cfg(target_os = "macos")]
const TRAY_PX: u32 = 44;
#[cfg(not(target_os = "macos"))]
const TRAY_PX: u32 = 32;

/// Last rasterised icon's cache key — the single redraw cache for the tray,
/// shared by every caller that can paint it: this module's own poll loop,
/// and every mutating command in `commands.rs` via [`publish_snapshot`] /
/// [`publish_switching`].
///
/// This used to be two independent caches: a `last_icon_key` local inside
/// [`run`]'s loop, and a separate `TrayCache` (identical in shape) managed
/// only for `lib.rs`'s one-off startup paint. Two caches for one on-screen
/// icon is a drift hazard by construction — consolidated onto *this* one,
/// `tauri`-managed rather than a plain local, because it is the only
/// representation reachable from both a long-lived spawned loop and a
/// short-lived command invocation; a local variable captured inside `run`'s
/// loop can never be reached from `commands.rs`.
#[derive(Default)]
pub(crate) struct TrayCache(Mutex<Option<u64>>);

/// Reflect `snapshot` outward: repaint the tray icon (honouring
/// [`TrayCache`]) and emit `snapshot://updated` for the frontend — the main
/// window and the tray popover both subscribe to this event instead of
/// polling the `snapshot` command themselves (see `src/lib/useSnapshot.ts`).
/// This loop remains the only thing that fetches usage on a schedule; that
/// contract is unaffected, since callers here are only ever republishing
/// data *this* loop or a mutating command already has in hand.
///
/// This is the ONLY function that knows how to publish a snapshot — [`run`]
/// below and every mutating command in `commands.rs` call this rather than
/// each reimplementing "paint tray + emit", which is exactly the kind of
/// drift [`TrayCache`]'s doc comment describes.
///
/// Never fails the caller: by the time a mutating command reaches this call,
/// its credential change has already succeeded — failing the command
/// afterwards over a tray/emit hiccup would misreport a successful switch as
/// a failed one. Both steps are logged-and-continued instead, and tray
/// rendering is wrapped in `catch_unwind` (matching the protection [`run`]
/// already relied on) since a panic here must not kill the poller task or
/// unwind through a `#[tauri::command]` either.
pub fn publish_snapshot(app: &AppHandle, snapshot: &Snapshot) {
    let spec = tray_spec_for(snapshot, ambient_theme_cached());
    paint_icon(app, spec);

    if let Err(e) = app.emit("snapshot://updated", snapshot) {
        log::debug!("publish_snapshot: emit snapshot failed: {e}");
    }
}

/// Paint the in-flight [`TrayState::Switching`] icon.
///
/// Call this before starting a mutation that changes the active account, so
/// the tray shows something is happening instead of sitting on the
/// pre-switch number for however long the swap takes — then call
/// [`publish_snapshot`] with the fresh snapshot once it completes. Same
/// never-fails-the-caller contract as [`publish_snapshot`].
pub fn publish_switching(app: &AppHandle) {
    let spec = IconSpec {
        utilisation: None,
        state: TrayState::Switching,
        spin: 0.0,
        theme: ambient_theme_cached(),
    };
    paint_icon(app, spec);
}

/// Store and publish a daemon phase if either it or the policy revision
/// changed. Hydration always reads the stored value, so an event can never be
/// emitted ahead of the authoritative snapshot it represents.
pub fn publish_daemon_status(
    app: &AppHandle,
    policy_revision: u64,
    phase: crate::runtime::DaemonPhase,
    now: DateTime<Utc>,
) {
    let state = app.state::<crate::commands::AppState>();
    let Some(status) = state.daemon_status.transition(policy_revision, phase, now) else {
        return;
    };
    if let Err(error) = app.emit("daemon://status", status) {
        log::debug!("publish_daemon_status: emit failed: {error}");
    }
}

/// Panic-safe wrapper around [`update_tray_icon`], shared by
/// [`publish_snapshot`], [`publish_switching`], and `lib.rs`'s one-time
/// startup placeholder paint (before the poller's first tick has produced a
/// real snapshot to publish).
pub(crate) fn paint_icon(app: &AppHandle, spec: IconSpec) {
    if std::panic::catch_unwind(AssertUnwindSafe(|| update_tray_icon(app, spec))).is_err() {
        log::warn!("paint_icon: tray render panicked; skipping this frame");
    }
}

/// Poll read-only, record history, paint the tray, emit events for the
/// frontend, and — only when `config.auto_switch_enabled` — perform a
/// decided switch. Runs forever; intended to be spawned once via
/// `tauri::async_runtime::spawn` by the caller. See the module doc comment
/// for the panic-safety and WSL-safety guarantees this loop upholds.
#[derive(Debug, Clone, PartialEq)]
enum LoopWake {
    Policy(crate::runtime::RuntimePolicy),
    Deadline,
    PolicyChannelClosed,
}

async fn wait_for_policy_or_deadline(
    policy_rx: &mut tokio::sync::watch::Receiver<crate::runtime::RuntimePolicy>,
    deadline: tokio::time::Instant,
) -> LoopWake {
    tokio::select! {
        changed = policy_rx.changed() => {
            if changed.is_err() {
                LoopWake::PolicyChannelClosed
            } else {
                // Clone and drop the watch borrow before the caller awaits
                // anything else. `borrow_and_update` also closes the race
                // between `changed` becoming ready and reading the value.
                let policy = policy_rx.borrow_and_update().clone();
                LoopWake::Policy(policy)
            }
        }
        _ = tokio::time::sleep_until(deadline) => LoopWake::Deadline,
    }
}

fn tokio_deadline_for(
    deadline: DateTime<Utc>,
    wall_now: DateTime<Utc>,
    instant_now: tokio::time::Instant,
) -> tokio::time::Instant {
    if deadline <= wall_now {
        return instant_now;
    }
    deadline
        .signed_duration_since(wall_now)
        .to_std()
        .ok()
        .and_then(|duration| instant_now.checked_add(duration))
        .unwrap_or(instant_now)
}

fn next_loop_deadline(
    state: &PollerLoopState,
    next_poll_at: tokio::time::Instant,
    wall_now: DateTime<Utc>,
    instant_now: tokio::time::Instant,
) -> tokio::time::Instant {
    let grace_at = state
        .daemon
        .pending
        .filter(|pending| {
            pending.policy_revision == state.policy.revision && !state.requires_fresh_snapshot
        })
        .map(|pending| tokio_deadline_for(pending.deadline, wall_now, instant_now));
    grace_at.map_or(next_poll_at, |deadline| deadline.min(next_poll_at))
}

pub async fn run(
    app: AppHandle,
    mut policy_rx: tokio::sync::watch::Receiver<crate::runtime::RuntimePolicy>,
) {
    let initial_policy = policy_rx.borrow_and_update().clone();
    log::info!(
        "poller: starting (threshold={}, auto_switch_enabled={})",
        initial_policy.threshold,
        initial_policy.auto_switch_enabled
    );

    let history = open_history(&app);
    let mut state = PollerLoopState::new(initial_policy);
    let mut prev_binding_pct: Option<f64> = None;
    let mut interval_s = poll_policy::DEFAULT_INTERVAL_S.max(poll_policy::MIN_INTERVAL_S);
    let mut next_poll_at = tokio::time::Instant::now();

    loop {
        let wall_now = Utc::now();
        let instant_now = tokio::time::Instant::now();
        let wake_at = next_loop_deadline(&state, next_poll_at, wall_now, instant_now);

        match wait_for_policy_or_deadline(&mut policy_rx, wake_at).await {
            LoopWake::Policy(policy) => {
                let now = Utc::now();
                if state.apply_policy(policy, now) {
                    let decision = state.decision_at(now);
                    publish_daemon_status(&app, state.policy.revision, decision.phase(), now);
                    // Enabling, resuming, or changing decision inputs must
                    // release the barrier only with a fresh observation.
                    if state.policy.auto_switch_enabled
                        && match state.policy.paused_until {
                            Some(until) => until <= now,
                            None => true,
                        }
                    {
                        next_poll_at = tokio::time::Instant::now();
                    }
                }
                continue;
            }
            LoopWake::PolicyChannelClosed => {
                log::info!("poller: policy channel closed; stopping");
                return;
            }
            LoopWake::Deadline => {}
        }

        let now = Utc::now();
        let due = state.decision_at(now);
        if let Decision::Switch {
            from,
            to,
            policy_revision,
        } = due
        {
            // The state machine checks this too; keep the guard adjacent to
            // the side effect so an old-policy decision cannot cross it.
            if policy_revision == state.policy.revision
                && state.policy.auto_switch_enabled
                && !state.requires_fresh_snapshot
            {
                publish_daemon_status(&app, policy_revision, due.phase(), now);
                let trusted = state.last_trusted_snapshot.clone();
                if let Some(snapshot) = trusted {
                    if perform_switch(&app, &snapshot, from, to).await {
                        state.complete_switch(from, now);
                    } else {
                        state.daemon.pending = None;
                        state.requires_fresh_snapshot = true;
                    }
                } else {
                    state.daemon.pending = None;
                    state.requires_fresh_snapshot = true;
                }
                next_poll_at = tokio::time::Instant::now();
                continue;
            }
        }

        if tokio::time::Instant::now() < next_poll_at {
            publish_daemon_status(&app, state.policy.revision, due.phase(), now);
            continue;
        }

        let visible = window_visible(&app);

        let snapshot = match fetch_snapshot_guarded().await {
            Some(s) => s,
            None => {
                let now = Utc::now();
                let decision = state.on_fetch_failed(now);
                publish_daemon_status(&app, state.policy.revision, decision.phase(), now);
                next_poll_at = tokio::time::Instant::now()
                    + Duration::from_secs_f64(interval_s.max(poll_policy::MIN_INTERVAL_S));
                continue;
            }
        };

        if let Some(store) = &history {
            match std::panic::catch_unwind(AssertUnwindSafe(|| store.record(&snapshot))) {
                Ok(Ok(n)) if n > 0 => log::debug!("poller: recorded {n} new sample(s)"),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => log::warn!("poller: history record failed: {e}"),
                Err(_) => log::warn!("poller: history record panicked; continuing"),
            }
        }

        // Paint the tray and push the update to the frontend — see
        // `publish_snapshot`'s doc comment; this loop is the only place that
        // fetches usage on a schedule, so republishing what it already
        // fetched here spends no extra budget against `poll_policy`'s floor.
        publish_snapshot(&app, &snapshot);

        let now = Utc::now();
        let decision = match std::panic::catch_unwind(AssertUnwindSafe(|| {
            state.on_snapshot(snapshot.clone(), now)
        })) {
            Ok(d) => d,
            Err(_) => {
                log::warn!("poller: state decision panicked; degrading this tick");
                Decision::Degraded {
                    reason: crate::runtime::DegradedReason::FetchFailed,
                }
            }
        };

        if let Err(e) = app.emit("poller://decision", &decision) {
            log::debug!("poller: emit decision failed: {e}");
        }
        publish_daemon_status(&app, state.policy.revision, decision.phase(), now);

        let mut sleep_override: Option<f64> = None;

        match &decision {
            Decision::Switch { from, to, .. } => {
                let (from, to) = (*from, *to);
                if perform_switch(&app, &snapshot, from, to).await {
                    state.complete_switch(from, now);
                    next_poll_at = tokio::time::Instant::now();
                    continue;
                }
            }
            Decision::Exhausted { earliest_reset } => {
                let wait = earliest_reset
                    .map(|reset| {
                        reset
                            .signed_duration_since(now)
                            .to_std()
                            .map_or(poll_policy::ACTIVE_MAX_INTERVAL_S, |wait| {
                                wait.as_secs_f64()
                            })
                            .max(poll_policy::DEFAULT_INTERVAL_S)
                    })
                    .unwrap_or(poll_policy::ACTIVE_MAX_INTERVAL_S);
                sleep_override = Some(wait.min(6.0 * 3600.0));
            }
            Decision::Disabled
            | Decision::Paused { .. }
            | Decision::Monitoring
            | Decision::Cooldown { .. }
            | Decision::Warning { .. }
            | Decision::Degraded { .. } => {}
        }

        let active = snapshot.active_account();
        let new_binding_pct = active.and_then(|a| a.binding_utilisation());
        let recent_failure = snapshot
            .environments
            .iter()
            .flat_map(|e| e.accounts.iter())
            .any(|a| a.usage_status == UsageStatus::Stale);
        let limiting_reset = active.and_then(active_limiting_reset_ts);
        let now_epoch = datetime_epoch_seconds(now);
        let earliest_future_reset =
            active.and_then(|a| active_earliest_future_reset_ts(a, now_epoch));

        let outcome = poll_policy::FetchOutcome {
            prev_interval_s: Some(interval_s),
            prev_binding_pct,
            new_binding_pct,
            is_active: true,
            threshold: state.policy.threshold,
            recent_429: recent_failure,
        };
        let (planned_poll_epoch, next_interval) =
            std::panic::catch_unwind(AssertUnwindSafe(|| {
                poll_policy::plan_after_fetch(
                    &outcome,
                    now_epoch,
                    pseudo_random_unit(),
                    earliest_future_reset,
                    limiting_reset,
                )
            }))
            .unwrap_or((now_epoch + interval_s, interval_s));
        interval_s = next_interval;
        prev_binding_pct = new_binding_pct;

        let mut sleep_s =
            sleep_override.unwrap_or_else(|| (planned_poll_epoch - now_epoch_s()).max(0.5));
        if !visible {
            // Pause the *urgent* cadence while nobody has the dashboard
            // open — but never fall fully silent, since the tray icon and
            // the (opt-in) auto-switch protection both still matter with
            // the window hidden, which is this app's normal resting state.
            sleep_s = sleep_s.max(poll_policy::ACTIVE_MAX_INTERVAL_S);
        }
        next_poll_at = tokio::time::Instant::now() + Duration::from_secs_f64(sleep_s);
    }
}

/// Perform a decided switch, gated by the caller having already checked
/// `auto_switch_enabled`. The switch runs in its own Tokio task so a panic is
/// returned as a `JoinError` instead of killing the daemon loop. Nothing here
/// holds a lock across the async validation or mutation.
async fn perform_switch(app: &AppHandle, snapshot: &Snapshot, from: u32, to: u32) -> bool {
    let Some(target) = find_account(snapshot, to).cloned() else {
        log::warn!("poller: decided to switch to account {to} but it is missing from the snapshot");
        return false;
    };

    let result = tokio::spawn(async move { switcher::switch_to(&target).await }).await;
    match result {
        Ok(Ok(())) => {
            let _ = app.emit(
                "poller://switch-performed",
                &serde_json::json!({ "from": from, "to": to, "ok": true }),
            );
            true
        }
        Ok(Err(e)) => {
            log::warn!("poller: switch_to({to}) failed: {e}");
            let _ = app.emit(
                "poller://switch-performed",
                &serde_json::json!({ "from": from, "to": to, "ok": false, "error": e.to_string() }),
            );
            false
        }
        Err(error) => {
            log::warn!("poller: switch_to({to}) task failed: {error}");
            false
        }
    }
}

/// Fetch a snapshot inside its own task so a panic anywhere in the read path
/// surfaces as a `JoinError` here instead of killing this loop. No lock of
/// any kind is held across this call — `read_snapshot` itself never holds
/// one across its network I/O (see its doc comment), and nothing above it in
/// this file holds one either.
async fn fetch_snapshot_guarded() -> Option<Snapshot> {
    match tokio::spawn(switcher::read_snapshot()).await {
        Ok(Ok(snap)) => Some(snap),
        Ok(Err(e)) => {
            log::warn!("poller: snapshot read failed: {e}");
            None
        }
        Err(join_err) => {
            log::warn!("poller: snapshot read task failed: {join_err}");
            None
        }
    }
}

fn window_visible(app: &AppHandle) -> bool {
    // Unknown defaults to visible: the safer failure mode is "poll a bit
    // more than necessary", not "go quiet because a query failed".
    app.get_webview_window("main")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(true)
}

fn find_account(snapshot: &Snapshot, number: u32) -> Option<&Account> {
    snapshot
        .environments
        .iter()
        .flat_map(|e| e.accounts.iter())
        .find(|a| a.number == number)
}

fn open_history(app: &AppHandle) -> Option<HistoryStore> {
    let dir = match app.path().app_data_dir() {
        Ok(d) => d,
        Err(e) => {
            log::warn!("poller: could not resolve app data dir: {e}");
            return None;
        }
    };
    match HistoryStore::open(&dir) {
        Ok(store) => Some(store),
        Err(e) => {
            log::warn!("poller: could not open history store: {e}");
            None
        }
    }
}

/// The tray icon to draw for this snapshot, against the given taskbar theme.
///
/// `theme` is passed in rather than probed here: detection shells out to the
/// registry on Windows, and this runs on every poll tick. The caller probes it
/// on a slow cadence and hands the result down.
fn tray_spec_for(snapshot: &Snapshot, theme: AmbientTheme) -> IconSpec {
    match snapshot.active_account() {
        None => IconSpec::unconfigured(theme),
        Some(a) => match a.binding_utilisation() {
            Some(u) => IconSpec::resting(u as f32, theme),
            None => IconSpec {
                utilisation: None,
                state: TrayState::Stale,
                spin: 0.0,
                theme,
            },
        },
    }
}

/// The taskbar theme, re-probed at most once a minute.
///
/// Detection spawns a `reg query` subprocess on Windows. Doing that every poll
/// tick would be wasteful for a value that changes when a user flips their OS
/// theme — rare, and a minute of staleness on the tray icon is imperceptible.
fn ambient_theme_cached() -> AmbientTheme {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    static CACHE: Mutex<Option<(Instant, AmbientTheme)>> = Mutex::new(None);
    const TTL: Duration = Duration::from_secs(60);

    let mut guard = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((at, theme)) = *guard {
        if at.elapsed() < TTL {
            return theme;
        }
    }
    let theme = AmbientTheme::detect();
    *guard = Some((Instant::now(), theme));
    theme
}

fn tray_tooltip(spec: IconSpec) -> String {
    match (spec.state, spec.utilisation) {
        (TrayState::Unconfigured, _) => "No accounts yet — click to add one".into(),
        (TrayState::Switching, _) => "Switching account…".into(),
        (TrayState::Stale, Some(p)) => format!("{p:.0}% · reading failed, showing last known"),
        (_, Some(p)) => format!("{p:.0}% of the binding window used"),
        (_, None) => "cc-logins".into(),
    }
}

/// Whether `spec` differs from the last drawn key — the pure half of the
/// redraw cache, deliberately kept separate from [`update_tray_icon`] so it
/// is directly testable without a `tauri::AppHandle`/managed [`TrayCache`].
/// Returns the cache key to store when a redraw is warranted (a real
/// change), or `None` for a no-op repeat that must not trigger a redraw.
fn should_redraw(spec: &IconSpec, last_key: Option<u64>) -> Option<u64> {
    let key = spec.cache_key();
    if last_key == Some(key) {
        None
    } else {
        Some(key)
    }
}

/// Repaint the tray icon for `spec`, using [`TrayCache`] (managed `tauri`
/// state — see its doc comment for why a plain local no longer suffices) and
/// [`should_redraw`] to skip a no-op repaint.
fn update_tray_icon(app: &AppHandle, spec: IconSpec) {
    let cache = app.state::<TrayCache>();
    {
        let mut last = cache.0.lock().unwrap_or_else(|p| p.into_inner());
        match should_redraw(&spec, *last) {
            Some(key) => *last = Some(key),
            None => return,
        }
    }

    let rgba = tray::render(spec, TRAY_PX);
    let image = Image::new_owned(rgba, TRAY_PX, TRAY_PX);
    if let Some(icon) = app.tray_by_id("main") {
        let _ = icon.set_icon(Some(image));
        let _ = icon.set_tooltip(Some(tray_tooltip(spec)));
    }
}

fn active_limiting_reset_ts(account: &Account) -> Option<f64> {
    let mut latest: Option<i64> = None;
    for (pct, resets_at) in account_reset_windows(account) {
        if pct >= 100.0 {
            if let Some(ts) = resets_at.and_then(parse_rfc3339_epoch) {
                latest = Some(latest.map_or(ts, |l| l.max(ts)));
            }
        }
    }
    latest.map(|t| t as f64)
}

fn active_earliest_future_reset_ts(account: &Account, now: f64) -> Option<f64> {
    let now_i = now as i64;
    let mut earliest: Option<i64> = None;
    for (_, resets_at) in account_reset_windows(account) {
        if let Some(ts) = resets_at.and_then(parse_rfc3339_epoch) {
            if ts > now_i {
                earliest = Some(earliest.map_or(ts, |e| e.min(ts)));
            }
        }
    }
    earliest.map(|t| t as f64)
}

fn now_epoch_s() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

/// A cheap, non-cryptographic jitter source (no `rand` dependency in this
/// crate): the sub-second fraction of the system clock. Good enough for
/// spreading poll ticks apart — see [`poll_policy::JITTER_FRAC`] — not used
/// for anything security-sensitive.
fn pseudo_random_unit() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos as f64) / 1_000_000_000.0
}

// ============================================================================
// 5. Tests.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EnvKind, EnvStatus, Environment, Usage, UsageWindow};
    use crate::settings::{Settings, Strategy as SettingsStrategy};

    // -- fixtures -------------------------------------------------------------

    fn window(pct: f64, resets_at: Option<&str>) -> UsageWindow {
        UsageWindow {
            pct,
            resets_at: resets_at.map(str::to_string),
            ..Default::default()
        }
    }

    fn account(
        number: u32,
        active: bool,
        seven_day_pct: Option<f64>,
        resets_at: Option<&str>,
    ) -> Account {
        let usage = seven_day_pct.map(|p| Usage {
            five_hour: None,
            seven_day: Some(window(p, resets_at)),
            scoped: None,
        });
        Account {
            number,
            email: format!("acct{number}@example.com"),
            active,
            usage_status: if usage.is_some() {
                crate::model::UsageStatus::Ok
            } else {
                crate::model::UsageStatus::Unknown
            },
            usage,
            ..Default::default()
        }
    }

    fn snapshot(accounts: Vec<Account>) -> Snapshot {
        Snapshot::new(vec![Environment {
            id: "native".into(),
            label: "Native".into(),
            path: String::new(),
            kind: EnvKind::Native,
            status: EnvStatus::Live,
            accounts,
            last_seen_seconds: None,
            has_credentials: None,
        }])
    }

    fn cfg() -> PollerConfig {
        PollerConfig {
            grace_seconds: 0.0,
            ..PollerConfig::default()
        }
    }

    const T0: f64 = 1_800_000_000.0;

    fn fixed_now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-28T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn runtime_policy(revision: u64) -> crate::runtime::RuntimePolicy {
        crate::runtime::RuntimePolicy::from_settings(
            revision,
            &Settings {
                auto_switch_enabled: true,
                threshold: 90,
                cooldown_seconds: 300,
                hysteresis_pct: 10,
                unhealthy_ticks: 3,
                strategy: SettingsStrategy::MostHeadroom,
                grace_seconds: 60,
                ..Settings::default()
            },
            fixed_now(),
        )
    }

    #[test]
    fn policy_change_disabled_and_active_pause_short_circuit_decisions() {
        let now = fixed_now();
        let disabled = crate::runtime::RuntimePolicy::from_settings(0, &Settings::default(), now);
        let mut state = PollerLoopState::new(disabled);
        assert_eq!(state.decision_at(now), Decision::Disabled);

        let until = now + chrono::Duration::hours(1);
        let paused = crate::runtime::RuntimePolicy::from_settings(
            1,
            &Settings {
                auto_switch_enabled: true,
                auto_switch_paused_until: Some(until),
                ..Settings::default()
            },
            now,
        );
        assert!(state.apply_policy(paused, now));
        assert_eq!(state.decision_at(now), Decision::Paused { until });
        assert_eq!(
            state.decision_at(until),
            Decision::Monitoring,
            "the pause boundary is exclusive"
        );
    }

    #[test]
    fn grace_uses_an_absolute_deadline_and_switches_at_the_exact_boundary() {
        let now = fixed_now();
        let mut state = PollerLoopState::new(runtime_policy(4));
        let snap = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, Some(10.0), None),
        ]);
        let deadline = now + chrono::Duration::seconds(60);

        assert_eq!(
            state.on_snapshot(snap, now),
            Decision::Warning {
                from: 1,
                to: 2,
                deadline
            }
        );
        assert_eq!(
            state.decision_at(deadline - chrono::Duration::milliseconds(1)),
            Decision::Warning {
                from: 1,
                to: 2,
                deadline
            }
        );
        assert_eq!(
            state.decision_at(deadline),
            Decision::Switch {
                from: 1,
                to: 2,
                policy_revision: 4
            }
        );
    }

    #[test]
    fn policy_change_cancels_pending_and_requires_a_fresh_snapshot() {
        let now = fixed_now();
        let mut state = PollerLoopState::new(runtime_policy(1));
        let snap = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, Some(10.0), None),
        ]);
        state.on_snapshot(snap.clone(), now);

        assert!(state.apply_policy(runtime_policy(2), now + chrono::Duration::seconds(10)));
        assert!(state.daemon.pending.is_none());
        assert!(state.requires_fresh_snapshot);
        assert_eq!(
            state.decision_at(now + chrono::Duration::minutes(5)),
            Decision::Monitoring,
            "an old-policy deadline cannot fire through the fresh-data barrier"
        );

        assert!(matches!(
            state.on_snapshot(snap, now + chrono::Duration::minutes(5)),
            Decision::Warning { .. }
        ));
        assert!(!state.requires_fresh_snapshot);
    }

    #[test]
    fn policy_change_rejects_older_or_duplicate_revisions() {
        let now = fixed_now();
        let mut state = PollerLoopState::new(runtime_policy(7));

        assert!(!state.apply_policy(runtime_policy(7), now));
        assert!(!state.apply_policy(runtime_policy(6), now));
        assert_eq!(state.policy.revision, 7);
    }

    #[test]
    fn grace_strategy_change_restarts_with_a_new_target_and_deadline() {
        let now = fixed_now();
        let mut state = PollerLoopState::new(runtime_policy(1));
        let snap = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, Some(10.0), None),
            account(3, false, Some(5.0), None),
        ]);
        assert!(matches!(
            state.on_snapshot(snap.clone(), now),
            Decision::Warning { to: 3, .. }
        ));

        let changed = crate::runtime::RuntimePolicy::from_settings(
            2,
            &Settings {
                auto_switch_enabled: true,
                strategy: SettingsStrategy::NextAvailable,
                grace_seconds: 60,
                ..Settings::default()
            },
            now,
        );
        state.apply_policy(changed, now + chrono::Duration::seconds(30));
        let restarted_at = now + chrono::Duration::seconds(31);

        assert_eq!(
            state.on_snapshot(snap, restarted_at),
            Decision::Warning {
                from: 1,
                to: 2,
                deadline: restarted_at + chrono::Duration::seconds(60)
            }
        );
    }

    #[test]
    fn grace_zero_switches_without_creating_a_pending_deadline() {
        let now = fixed_now();
        let mut settings = Settings {
            auto_switch_enabled: true,
            grace_seconds: 0,
            ..Settings::default()
        };
        settings.threshold = 90;
        let policy = crate::runtime::RuntimePolicy::from_settings(5, &settings, now);
        let mut state = PollerLoopState::new(policy);
        let snap = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, Some(10.0), None),
        ]);

        assert_eq!(
            state.on_snapshot(snap, now),
            Decision::Switch {
                from: 1,
                to: 2,
                policy_revision: 5
            }
        );
        assert!(state.daemon.pending.is_none());
    }

    #[test]
    fn grace_exact_cooldown_boundary_releases_the_candidate() {
        let now = fixed_now();
        let mut state = PollerLoopState::new(runtime_policy(1));
        state.daemon.last_switch_at = Some(now);
        let snap = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, Some(10.0), None),
        ]);
        let until = now + chrono::Duration::seconds(300);

        assert_eq!(
            state.on_snapshot(snap.clone(), until - chrono::Duration::milliseconds(1)),
            Decision::Cooldown { until }
        );
        assert!(matches!(
            state.on_snapshot(snap, until),
            Decision::Warning { .. }
        ));
    }

    #[test]
    fn policy_change_unknown_usage_is_degraded_and_failed_fetch_keeps_the_barrier() {
        let now = fixed_now();
        let mut state = PollerLoopState::new(runtime_policy(1));
        let unknown = snapshot(vec![
            account(1, true, None, None),
            account(2, false, Some(10.0), None),
        ]);

        assert_eq!(
            state.on_snapshot(unknown, now),
            Decision::Degraded {
                reason: crate::runtime::DegradedReason::UsageUnknown
            }
        );
        state.apply_policy(runtime_policy(2), now);
        assert_eq!(
            state.on_fetch_failed(now),
            Decision::Degraded {
                reason: crate::runtime::DegradedReason::FetchFailed
            }
        );
        assert!(state.requires_fresh_snapshot);
    }

    #[test]
    fn policy_change_proven_exhaustion_carries_the_earliest_reset() {
        let now = fixed_now();
        let mut state = PollerLoopState::new(runtime_policy(1));
        let snap = snapshot(vec![
            account(1, true, Some(100.0), Some("2026-08-01T00:00:00Z")),
            account(2, false, Some(100.0), Some("2026-08-02T00:00:00Z")),
        ]);

        assert_eq!(
            state.on_snapshot(snap, now),
            Decision::Exhausted {
                earliest_reset: Some(
                    DateTime::parse_from_rfc3339("2026-08-01T00:00:00Z")
                        .unwrap()
                        .with_timezone(&Utc)
                )
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn policy_update_wakes_a_long_sleep_immediately() {
        let (tx, mut rx) = tokio::sync::watch::channel(runtime_policy(1));
        let sleeper = tokio::spawn(async move {
            wait_for_policy_or_deadline(
                &mut rx,
                tokio::time::Instant::now() + Duration::from_secs(3600),
            )
            .await
        });
        tokio::task::yield_now().await;

        tx.send_replace(runtime_policy(2));

        assert!(matches!(sleeper.await.unwrap(), LoopWake::Policy(policy) if policy.revision == 2));
    }

    #[tokio::test(start_paused = true)]
    async fn policy_update_coalesces_rapid_changes_to_the_latest_complete_value() {
        let (tx, mut rx) = tokio::sync::watch::channel(runtime_policy(1));
        tx.send_replace(runtime_policy(2));
        tx.send_replace(runtime_policy(3));

        let wake = wait_for_policy_or_deadline(
            &mut rx,
            tokio::time::Instant::now() + Duration::from_secs(3600),
        )
        .await;

        assert!(matches!(wake, LoopWake::Policy(policy) if policy.revision == 3));
    }

    #[tokio::test(start_paused = true)]
    async fn policy_update_disable_and_pause_cancel_pending_before_the_old_deadline() {
        let now = fixed_now();
        let mut state = PollerLoopState::new(runtime_policy(1));
        state.on_snapshot(
            snapshot(vec![
                account(1, true, Some(95.0), None),
                account(2, false, Some(10.0), None),
            ]),
            now,
        );
        assert!(state.daemon.pending.is_some());

        let disabled = crate::runtime::RuntimePolicy::from_settings(2, &Settings::default(), now);
        state.apply_policy(disabled, now);
        assert!(state.daemon.pending.is_none());
        assert_eq!(
            state.decision_at(now + chrono::Duration::hours(1)),
            Decision::Disabled
        );

        let until = now + chrono::Duration::hours(2);
        let paused = crate::runtime::RuntimePolicy::from_settings(
            3,
            &Settings {
                auto_switch_enabled: true,
                auto_switch_paused_until: Some(until),
                ..Settings::default()
            },
            now,
        );
        state.apply_policy(paused, now);
        assert_eq!(state.decision_at(now), Decision::Paused { until });
    }

    #[tokio::test(start_paused = true)]
    async fn policy_update_enable_and_resume_keep_the_fresh_snapshot_barrier() {
        let now = fixed_now();
        let disabled = crate::runtime::RuntimePolicy::from_settings(1, &Settings::default(), now);
        let mut state = PollerLoopState::new(disabled);

        state.apply_policy(runtime_policy(2), now);

        assert!(state.requires_fresh_snapshot);
        assert_eq!(
            state.decision_at(now + chrono::Duration::hours(1)),
            Decision::Monitoring
        );
        assert_eq!(
            state.on_fetch_failed(now),
            Decision::Degraded {
                reason: crate::runtime::DegradedReason::FetchFailed
            }
        );
        assert!(state.requires_fresh_snapshot);
    }

    #[tokio::test(start_paused = true)]
    async fn grace_deadline_wakes_without_waiting_for_or_triggering_a_fetch() {
        let now = fixed_now();
        let mut state = PollerLoopState::new(runtime_policy(9));
        state.on_snapshot(
            snapshot(vec![
                account(1, true, Some(95.0), None),
                account(2, false, Some(10.0), None),
            ]),
            now,
        );
        let deadline = state.daemon.pending.unwrap().deadline;
        let (_tx, mut rx) = tokio::sync::watch::channel(runtime_policy(9));
        let wake_at = tokio_deadline_for(deadline, now, tokio::time::Instant::now());
        let sleeper =
            tokio::spawn(async move { wait_for_policy_or_deadline(&mut rx, wake_at).await });
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(!sleeper.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;

        assert_eq!(sleeper.await.unwrap(), LoopWake::Deadline);
        assert!(matches!(
            state.decision_at(deadline),
            Decision::Switch {
                policy_revision: 9,
                ..
            }
        ));
    }

    // -- basic hold / no-active-account ----------------------------------------

    #[test]
    fn hold_when_no_active_account() {
        let snap = snapshot(vec![account(1, false, Some(10.0), None)]);
        let decision = decide(&snap, &cfg(), &LegacyDaemonState::default(), T0);
        assert_eq!(decision, LegacyDecision::Hold);
    }

    #[test]
    fn hold_when_active_usage_below_threshold() {
        let snap = snapshot(vec![
            account(1, true, Some(50.0), None),
            account(2, false, Some(10.0), None),
        ]);
        let decision = decide(&snap, &cfg(), &LegacyDaemonState::default(), T0);
        assert_eq!(decision, LegacyDecision::Hold);
    }

    #[test]
    fn hold_when_active_usage_unknown_and_under_the_unhealthy_tick_count() {
        let snap = snapshot(vec![
            account(1, true, None, None),
            account(2, false, Some(10.0), None),
        ]);
        let config = cfg();
        let state = LegacyDaemonState {
            unhealthy_ticks: config.unhealthy_ticks - 1,
            ..Default::default()
        };
        assert_eq!(decide(&snap, &config, &state, T0), LegacyDecision::Hold);
    }

    // -- rule 1: unknown usage is never auto-skipped or treated as zero -------

    #[test]
    fn unknown_usage_candidate_is_not_chosen_over_a_known_one() {
        let snap = snapshot(vec![
            account(1, true, Some(95.0), None),  // active, over threshold
            account(2, false, None, None),       // unknown usage — must not win by default
            account(3, false, Some(20.0), None), // headroom 80 — the real winner
        ]);
        let decision = decide(&snap, &cfg(), &LegacyDaemonState::default(), T0);
        assert_eq!(decision, LegacyDecision::Switch { from: 1, to: 3 });
    }

    #[test]
    fn unknown_usage_active_account_triggers_failover_after_enough_unhealthy_ticks() {
        let snap = snapshot(vec![
            account(1, true, None, None),
            account(2, false, Some(20.0), None),
        ]);
        let config = cfg();
        let state = LegacyDaemonState {
            unhealthy_ticks: config.unhealthy_ticks,
            ..Default::default()
        };
        assert_eq!(
            decide(&snap, &config, &state, T0),
            LegacyDecision::Switch { from: 1, to: 2 }
        );
    }

    #[test]
    fn unknown_only_candidates_hold_rather_than_pick_or_declare_exhausted() {
        let snap = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, None, None),
            account(3, false, None, None),
        ]);
        // No known-headroom candidate exists, but not everyone is *known*
        // to be at their limit either — this must never resolve to
        // Exhausted (that would be a false "give up").
        assert_eq!(
            decide(&snap, &cfg(), &LegacyDaemonState::default(), T0),
            LegacyDecision::Hold
        );
    }

    // -- rule 2: cooldown -------------------------------------------------------

    #[test]
    fn cooldown_blocks_a_switch_within_the_window() {
        let snap = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, Some(10.0), None),
        ]);
        let config = cfg();
        let state = LegacyDaemonState {
            last_switch_at: Some(T0 - 10.0),
            ..Default::default()
        };
        assert_eq!(decide(&snap, &config, &state, T0), LegacyDecision::Hold);
    }

    #[test]
    fn cooldown_expires_and_allows_the_switch() {
        let snap = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, Some(10.0), None),
        ]);
        let config = cfg();
        let state = LegacyDaemonState {
            last_switch_at: Some(T0 - config.cooldown_seconds - 1.0),
            ..Default::default()
        };
        assert_eq!(
            decide(&snap, &config, &state, T0),
            LegacyDecision::Switch { from: 1, to: 2 }
        );
    }

    #[test]
    fn cooldown_boundary_is_exclusive() {
        let snap = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, Some(10.0), None),
        ]);
        let config = cfg();
        // Exactly cooldown_seconds ago: no longer "within" the window.
        let state = LegacyDaemonState {
            last_switch_at: Some(T0 - config.cooldown_seconds),
            ..Default::default()
        };
        assert_eq!(
            decide(&snap, &config, &state, T0),
            LegacyDecision::Switch { from: 1, to: 2 }
        );
    }

    // -- rule 3: hysteresis ------------------------------------------------------

    #[test]
    fn hysteresis_blocks_switching_back_to_the_account_just_left() {
        let config = cfg(); // threshold 90, hysteresis 10 -> floor 80
        let snap = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, Some(85.0), None), // utilisation 85 > floor 80: still blocked
            account(3, false, Some(50.0), None), // headroom 50 — should win instead
        ]);
        let state = LegacyDaemonState {
            last_switch_from: Some(2),
            ..Default::default()
        };
        assert_eq!(
            decide(&snap, &config, &state, T0),
            LegacyDecision::Switch { from: 1, to: 3 }
        );
    }

    #[test]
    fn hysteresis_releases_once_dropped_far_enough_below_threshold() {
        let config = cfg();
        let snap = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, Some(75.0), None), // utilisation 75 <= floor 80: eligible again
        ]);
        let state = LegacyDaemonState {
            last_switch_from: Some(2),
            ..Default::default()
        };
        assert_eq!(
            decide(&snap, &config, &state, T0),
            LegacyDecision::Switch { from: 1, to: 2 }
        );
    }

    #[test]
    fn hysteresis_does_not_affect_unrelated_accounts_with_unknown_usage() {
        let config = cfg();
        let snap = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, Some(85.0), None), // still hysteresis-blocked
            account(3, false, None, None),       // unrelated, unknown — fine to remain eligible
        ]);
        let state = LegacyDaemonState {
            last_switch_from: Some(2),
            ..Default::default()
        };
        // Only account 3 is left after excluding 2, but its usage is
        // unknown, so pick_target can't prove it's the best target either —
        // this must stay Hold, not Exhausted (rule 4/1 interaction) and
        // must never silently re-pick account 2.
        assert_eq!(decide(&snap, &config, &state, T0), LegacyDecision::Hold);
    }

    #[test]
    fn hysteresis_blocked_account_with_headroom_prevents_a_false_exhausted() {
        let config = cfg();
        let snap = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, Some(85.0), None), // hysteresis-blocked, but has real headroom
        ]);
        let state = LegacyDaemonState {
            last_switch_from: Some(2),
            ..Default::default()
        };
        // Not a valid *target* this tick, but the situation is not
        // exhausted — a real option exists, just temporarily blocked.
        assert_eq!(decide(&snap, &config, &state, T0), LegacyDecision::Hold);
    }

    // -- rule 4: all-exhausted ----------------------------------------------------

    #[test]
    fn exhausted_when_every_relevant_account_is_known_to_be_at_its_limit() {
        let snap = snapshot(vec![
            account(1, true, Some(100.0), Some("2026-08-01T00:00:00Z")),
            account(2, false, Some(100.0), Some("2026-08-02T00:00:00Z")),
        ]);
        let decision = decide(&snap, &cfg(), &LegacyDaemonState::default(), T0);
        assert_eq!(
            decision,
            LegacyDecision::Exhausted {
                earliest_reset: Some("2026-08-01T00:00:00+00:00".to_string())
            }
        );
    }

    #[test]
    fn exhausted_reset_is_none_when_unprovable() {
        let snap = snapshot(vec![
            account(1, true, Some(100.0), None), // at limit, no reset time known
            account(2, false, Some(100.0), Some("2026-08-02T00:00:00Z")),
        ]);
        let decision = decide(&snap, &cfg(), &LegacyDaemonState::default(), T0);
        assert_eq!(
            decision,
            LegacyDecision::Exhausted {
                earliest_reset: None
            }
        );
    }

    #[test]
    fn not_exhausted_while_any_relevant_account_usage_is_unknown() {
        let snap = snapshot(vec![
            account(1, true, Some(100.0), Some("2026-08-01T00:00:00Z")),
            account(2, false, None, None), // unknown — can't prove it's also at its limit
        ]);
        assert_eq!(
            decide(&snap, &cfg(), &LegacyDaemonState::default(), T0),
            LegacyDecision::Hold
        );
    }

    #[test]
    fn never_spins_repeated_ticks_on_a_fully_exhausted_state_stay_exhausted() {
        let snap = snapshot(vec![
            account(1, true, Some(100.0), Some("2026-08-01T00:00:00Z")),
            account(2, false, Some(100.0), Some("2026-08-02T00:00:00Z")),
        ]);
        let config = cfg();
        let mut state = LegacyDaemonState::default();
        for tick in 0..5 {
            let decision = decide(&snap, &config, &state, T0 + tick as f64 * 60.0);
            assert!(
                matches!(decision, LegacyDecision::Exhausted { .. }),
                "tick {tick} was {decision:?}"
            );
            state.pending = None; // what `run` would do after an Exhausted tick
        }
    }

    // -- rule 5: unhealthy ticks ---------------------------------------------------

    #[test]
    fn unhealthy_tick_counter_resets_on_a_known_reading() {
        assert_eq!(next_unhealthy_ticks(2, true), 0);
        assert_eq!(next_unhealthy_ticks(0, false), 1);
        assert_eq!(next_unhealthy_ticks(2, false), 3);
    }

    #[test]
    fn failover_does_not_trigger_before_the_configured_tick_count() {
        let snap = snapshot(vec![
            account(1, true, None, None),
            account(2, false, Some(10.0), None),
        ]);
        let config = PollerConfig {
            unhealthy_ticks: 3,
            ..cfg()
        };
        for n in 0..config.unhealthy_ticks {
            let state = LegacyDaemonState {
                unhealthy_ticks: n,
                ..Default::default()
            };
            assert_eq!(
                decide(&snap, &config, &state, T0),
                LegacyDecision::Hold,
                "unhealthy_ticks={n}"
            );
        }
        let state = LegacyDaemonState {
            unhealthy_ticks: config.unhealthy_ticks,
            ..Default::default()
        };
        assert_eq!(
            decide(&snap, &config, &state, T0),
            LegacyDecision::Switch { from: 1, to: 2 }
        );
    }

    // -- rule 6: grace countdown ----------------------------------------------------

    #[test]
    fn grace_zero_switches_on_the_same_tick() {
        let snap = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, Some(10.0), None),
        ]);
        let config = PollerConfig {
            grace_seconds: 0.0,
            ..PollerConfig::default()
        };
        assert_eq!(
            decide(&snap, &config, &LegacyDaemonState::default(), T0),
            LegacyDecision::Switch { from: 1, to: 2 }
        );
    }

    #[test]
    fn grace_warns_first_then_switches_once_elapsed() {
        let snap = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, Some(10.0), None),
        ]);
        let config = PollerConfig {
            grace_seconds: 60.0,
            ..PollerConfig::default()
        };

        // Tick 1: nothing pending yet -> full countdown starts.
        let decision = decide(&snap, &config, &LegacyDaemonState::default(), T0);
        assert_eq!(
            decision,
            LegacyDecision::Warn {
                account: 2,
                seconds_left: 60.0
            }
        );

        // Caller records the pending countdown (mirrors what `run` does).
        let state = LegacyDaemonState {
            pending: Some(LegacyPendingSwitch {
                from: 1,
                to: 2,
                decided_at: T0,
            }),
            ..Default::default()
        };

        // Tick 2, 30s later: half the countdown remains.
        let decision = decide(&snap, &config, &state, T0 + 30.0);
        assert_eq!(
            decision,
            LegacyDecision::Warn {
                account: 2,
                seconds_left: 30.0
            }
        );

        // Tick 3, grace fully elapsed: switch.
        let decision = decide(&snap, &config, &state, T0 + 60.0);
        assert_eq!(decision, LegacyDecision::Switch { from: 1, to: 2 });

        // Comfortably past grace: still switches (not stuck waiting for an
        // exact tick boundary).
        let decision = decide(&snap, &config, &state, T0 + 120.0);
        assert_eq!(decision, LegacyDecision::Switch { from: 1, to: 2 });
    }

    #[test]
    fn grace_countdown_restarts_when_the_target_changes_mid_countdown() {
        let config = PollerConfig {
            grace_seconds: 60.0,
            ..PollerConfig::default()
        };
        // Initially account 3 has the most headroom.
        let snap1 = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, Some(50.0), None),
            account(3, false, Some(10.0), None), // headroom 90 — best
        ]);
        let decision = decide(&snap1, &config, &LegacyDaemonState::default(), T0);
        assert_eq!(
            decision,
            LegacyDecision::Warn {
                account: 3,
                seconds_left: 60.0
            }
        );

        let pending = LegacyPendingSwitch {
            from: 1,
            to: 3,
            decided_at: T0,
        };
        let state = LegacyDaemonState {
            pending: Some(pending),
            ..Default::default()
        };

        // Now account 2 pulls ahead before the countdown finished.
        let snap2 = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, Some(5.0), None), // headroom 95 — new best
            account(3, false, Some(10.0), None),
        ]);
        let decision = decide(&snap2, &config, &state, T0 + 40.0);
        // Restarts at the full grace period for the new target, not
        // inheriting the old target's elapsed time.
        assert_eq!(
            decision,
            LegacyDecision::Warn {
                account: 2,
                seconds_left: 60.0
            }
        );
    }

    #[test]
    fn hold_after_recovery_cancels_an_in_progress_warn() {
        // The `run` loop clears `state.pending` whenever `decide` returns
        // Hold; this test documents/exercises the decide()-side half of
        // that contract: once utilisation drops back under threshold,
        // decide() itself no longer asks for a Warn/Switch regardless of
        // what was pending.
        let config = PollerConfig {
            grace_seconds: 60.0,
            ..PollerConfig::default()
        };
        let pending = LegacyPendingSwitch {
            from: 1,
            to: 2,
            decided_at: T0,
        };
        let state = LegacyDaemonState {
            pending: Some(pending),
            ..Default::default()
        };

        let recovered = snapshot(vec![
            account(1, true, Some(40.0), None),
            account(2, false, Some(10.0), None),
        ]);
        assert_eq!(
            decide(&recovered, &config, &state, T0 + 10.0),
            LegacyDecision::Hold
        );
    }

    // -- strategy pass-through -------------------------------------------------------

    #[test]
    fn decide_honours_the_configured_strategy() {
        let snap = snapshot(vec![
            account(1, true, Some(95.0), None),
            account(2, false, Some(10.0), None), // first in order, headroom 90
            account(3, false, Some(5.0), None),  // more headroom (95), but not first
        ]);
        let config = PollerConfig {
            grace_seconds: 0.0,
            strategy: Strategy::NextAvailable,
            ..PollerConfig::default()
        };
        assert_eq!(
            decide(&snap, &config, &LegacyDaemonState::default(), T0),
            LegacyDecision::Switch { from: 1, to: 2 }
        );

        let config = PollerConfig {
            grace_seconds: 0.0,
            strategy: Strategy::MostHeadroom,
            ..PollerConfig::default()
        };
        assert_eq!(
            decide(&snap, &config, &LegacyDaemonState::default(), T0),
            LegacyDecision::Switch { from: 1, to: 3 }
        );
    }

    // -- disabled/re-login accounts never targeted or counted ------------------------

    #[test]
    fn disabled_and_relogin_accounts_are_ignored_for_targeting_and_exhaustion() {
        let mut disabled = account(2, false, Some(0.0), None);
        disabled.usage_status = crate::model::UsageStatus::Disabled;
        let mut dead = account(3, false, Some(0.0), None);
        dead.usage_status = crate::model::UsageStatus::ReloginRequired;
        let snap = snapshot(vec![
            account(1, true, Some(100.0), Some("2026-08-01T00:00:00Z")),
            disabled,
            dead,
        ]);
        // Only the active account is "relevant" (the disabled one is
        // excluded from is_switchable), and it's known at-limit with a
        // reset time -> Exhausted, not blocked forever by the disabled seat.
        let decision = decide(&snap, &cfg(), &LegacyDaemonState::default(), T0);
        assert_eq!(
            decision,
            LegacyDecision::Exhausted {
                earliest_reset: Some("2026-08-01T00:00:00+00:00".to_string())
            }
        );
    }

    // -- poll_policy --------------------------------------------------------------------

    mod poll_policy_tests {
        use super::poll_policy::*;

        fn base_outcome() -> FetchOutcome {
            FetchOutcome {
                prev_interval_s: Some(MIN_INTERVAL_S),
                prev_binding_pct: Some(50.0),
                new_binding_pct: Some(50.0),
                is_active: true,
                threshold: 90.0,
                recent_429: false,
            }
        }

        #[test]
        fn no_movement_backs_off_by_half_again_capped_at_ceiling() {
            let outcome = FetchOutcome {
                prev_interval_s: Some(ACTIVE_MAX_INTERVAL_S),
                ..base_outcome()
            };
            let (_, interval) = plan_after_fetch(&outcome, 0.0, 0.5, None, None);
            assert_eq!(
                interval, ACTIVE_MAX_INTERVAL_S,
                "must not exceed the ceiling"
            );
        }

        #[test]
        fn movement_halves_the_interval_floored_at_min() {
            let outcome = FetchOutcome {
                prev_interval_s: Some(200.0),
                new_binding_pct: Some(55.0),
                ..base_outcome()
            };
            let (_, interval) = plan_after_fetch(&outcome, 0.0, 0.5, None, None);
            assert_eq!(interval, 100.0f64.max(MIN_INTERVAL_S));
        }

        #[test]
        fn movement_never_drops_below_the_floor() {
            let outcome = FetchOutcome {
                prev_interval_s: Some(MIN_INTERVAL_S),
                new_binding_pct: Some(55.0),
                ..base_outcome()
            };
            let (_, interval) = plan_after_fetch(&outcome, 0.0, 0.5, None, None);
            assert_eq!(interval, MIN_INTERVAL_S);
        }

        #[test]
        fn urgent_mode_when_active_moving_and_near_threshold() {
            let outcome = FetchOutcome {
                prev_interval_s: Some(200.0),
                prev_binding_pct: Some(74.0),
                new_binding_pct: Some(76.0), // within ESCALATION_MARGIN_PCT(15) of threshold 90
                ..base_outcome()
            };
            let (_, interval) = plan_after_fetch(&outcome, 0.0, 0.5, None, None);
            assert_eq!(interval, URGENT_INTERVAL_S);
        }

        #[test]
        fn urgent_mode_does_not_trigger_when_not_moving() {
            let outcome = FetchOutcome {
                prev_interval_s: Some(200.0),
                prev_binding_pct: Some(76.0),
                new_binding_pct: Some(76.0), // near threshold but unchanged
                ..base_outcome()
            };
            let (_, interval) = plan_after_fetch(&outcome, 0.0, 0.5, None, None);
            assert_ne!(interval, URGENT_INTERVAL_S);
        }

        #[test]
        fn recent_429_floors_the_interval_even_on_the_first_backoff() {
            let outcome = FetchOutcome {
                prev_interval_s: Some(MIN_INTERVAL_S),
                recent_429: true,
                ..base_outcome()
            };
            let (_, interval) = plan_after_fetch(&outcome, 0.0, 0.5, None, None);
            assert!(interval >= POST_429_MIN_INTERVAL_S);
        }

        #[test]
        fn recent_429_grows_multiplicatively_and_caps_at_the_429_ceiling() {
            let outcome = FetchOutcome {
                prev_interval_s: Some(POST_429_MAX_INTERVAL_S),
                recent_429: true,
                ..base_outcome()
            };
            let (_, interval) = plan_after_fetch(&outcome, 0.0, 0.5, None, None);
            assert_eq!(interval, POST_429_MAX_INTERVAL_S);
        }

        #[test]
        fn jitter_moves_the_next_poll_time_within_the_configured_fraction() {
            let outcome = base_outcome();
            let (low, interval_low) = plan_after_fetch(&outcome, 1000.0, 0.0, None, None);
            let (high, interval_high) = plan_after_fetch(&outcome, 1000.0, 1.0, None, None);
            assert_eq!(
                interval_low, interval_high,
                "jitter must not change the learned interval"
            );
            assert!(
                low < 1000.0 + interval_low,
                "min jitter sample pulls the schedule earlier"
            );
            assert!(
                high > 1000.0 + interval_high * 0.99,
                "max jitter sample pushes the schedule later"
            );
        }

        #[test]
        fn known_at_limit_pushes_next_poll_out_to_a_later_reset_time() {
            // An account known to be at its limit cannot change before its
            // reset, so a reset further out than the normal schedule pushes
            // the next poll out to it — no point polling in between.
            let outcome = FetchOutcome {
                new_binding_pct: Some(100.0),
                ..base_outcome()
            };
            let (next_poll, interval) =
                plan_after_fetch(&outcome, 0.0, 0.5, None, Some(1_000_000.0));
            assert!(
                interval < 1_000_000.0,
                "sanity: the reset is far beyond the normal cadence"
            );
            assert_eq!(next_poll, 1_000_000.0);
        }

        #[test]
        fn known_at_limit_does_not_move_the_schedule_earlier_than_planned() {
            let outcome = FetchOutcome {
                new_binding_pct: Some(100.0),
                ..base_outcome()
            };
            let (next_poll, interval) = plan_after_fetch(&outcome, 0.0, 0.5, None, Some(1.0));
            // reset_ts (1.0) is earlier than the planned poll -> ignored,
            // the normal interval-based schedule wins.
            assert_eq!(next_poll, interval);
        }

        #[test]
        fn unknown_utilisation_never_consults_the_at_limit_reset_clamp() {
            let outcome = FetchOutcome {
                new_binding_pct: None,
                ..base_outcome()
            };
            let (next_poll, _) = plan_after_fetch(&outcome, 1000.0, 0.5, None, Some(1.0));
            assert!(
                next_poll > 1000.0,
                "must not clamp to a reset time when utilisation is unknown"
            );
        }

        #[test]
        fn earliest_future_reset_pulls_the_next_poll_in_with_slack() {
            let outcome = FetchOutcome {
                new_binding_pct: Some(50.0),
                ..base_outcome()
            };
            let (next_poll, interval) = plan_after_fetch(&outcome, 0.0, 0.5, Some(10.0), None);
            assert_eq!(next_poll, 10.0 + RESET_SLACK_S);
            assert!(interval > 0.0);
        }
    }

    // -- publish path: tray-spec selection (pure) --------------------------------------
    //
    // `tray_spec_for` and `should_redraw` are exactly the parts of
    // `publish_snapshot`/`update_tray_icon` that don't need a
    // `tauri::AppHandle` — no Tauri app is spawned by any test in this file.

    mod tray_spec_tests {
        use super::*;

        #[test]
        fn no_active_account_is_unconfigured() {
            let snap = snapshot(vec![account(1, false, Some(10.0), None)]);
            let spec = tray_spec_for(&snap, AmbientTheme::Dark);
            assert_eq!(spec.state, TrayState::Unconfigured);
            assert_eq!(spec.utilisation, None);
        }

        #[test]
        fn active_account_with_known_usage_is_resting_at_that_utilisation() {
            let snap = snapshot(vec![account(1, true, Some(30.0), None)]);
            let spec = tray_spec_for(&snap, AmbientTheme::Dark);
            // headroom = 100 - 30 = 70 -> binding utilisation 30.
            assert_eq!(spec.utilisation, Some(30.0));
            assert_eq!(spec.state, TrayState::Ok);
        }

        #[test]
        fn active_account_with_unknown_usage_is_stale_not_unconfigured() {
            let snap = snapshot(vec![account(1, true, None, None)]);
            let spec = tray_spec_for(&snap, AmbientTheme::Dark);
            assert_eq!(spec.state, TrayState::Stale);
            assert_eq!(spec.utilisation, None);
        }

        #[test]
        fn high_utilisation_classifies_as_critical() {
            let snap = snapshot(vec![account(1, true, Some(95.0), None)]);
            let spec = tray_spec_for(&snap, AmbientTheme::Dark);
            assert_eq!(spec.utilisation, Some(95.0));
            assert_eq!(spec.state, TrayState::Critical);
        }

        #[test]
        fn theme_passes_through_unchanged() {
            let snap = snapshot(vec![account(1, true, Some(10.0), None)]);
            assert_eq!(
                tray_spec_for(&snap, AmbientTheme::Light).theme,
                AmbientTheme::Light
            );
            assert_eq!(
                tray_spec_for(&snap, AmbientTheme::Dark).theme,
                AmbientTheme::Dark
            );
        }
    }

    // -- publish path: redraw cache (pure) --------------------------------------------

    mod redraw_cache_tests {
        use super::*;

        #[test]
        fn first_paint_with_no_prior_key_always_redraws() {
            let spec = IconSpec::resting(50.0, AmbientTheme::Dark);
            assert_eq!(should_redraw(&spec, None), Some(spec.cache_key()));
        }

        #[test]
        fn identical_spec_suppresses_a_repeat_redraw() {
            let spec = IconSpec::resting(50.0, AmbientTheme::Dark);
            let key = spec.cache_key();
            assert_eq!(
                should_redraw(&spec, Some(key)),
                None,
                "a no-op repeat must not redraw"
            );
        }

        #[test]
        fn a_real_change_still_redraws_even_with_a_prior_key_set() {
            let before = IconSpec::resting(50.0, AmbientTheme::Dark);
            let after = IconSpec::resting(51.0, AmbientTheme::Dark);
            let result = should_redraw(&after, Some(before.cache_key()));
            assert_eq!(
                result,
                Some(after.cache_key()),
                "a real change must still trigger a redraw"
            );
        }

        #[test]
        fn switching_state_is_a_real_change_from_resting() {
            // The whole point of exposing `State::Switching`: it must not be
            // coalesced away by the cache just because the numeric
            // utilisation happens to match the account being switched away
            // from.
            let resting = IconSpec::resting(50.0, AmbientTheme::Dark);
            let switching = IconSpec {
                utilisation: None,
                state: TrayState::Switching,
                spin: 0.0,
                theme: AmbientTheme::Dark,
            };
            assert_ne!(resting.cache_key(), switching.cache_key());
            assert_eq!(
                should_redraw(&switching, Some(resting.cache_key())),
                Some(switching.cache_key())
            );
        }
    }
}
