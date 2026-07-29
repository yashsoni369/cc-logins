/**
 * Pooled runway — how long everything the user owns lasts at the current burn.
 *
 * This is the Dashboard's headline figure and the only number on that screen
 * that is a *projection* rather than a measurement. PRODUCT.md's rule — never
 * lie about freshness, degrade to labelled-stale rather than blank or silently
 * wrong — is therefore built into the return type: an estimate always arrives
 * with `degraded` and `contributing` attached, so a caller cannot render the
 * number without also being handed the reasons to distrust it.
 *
 * The model is that quota is spent *sequentially*: one person works at one
 * speed, only the installed credential burns, and when it hits its limit the
 * app switches to the next account, which then burns at that same speed. So
 * the pool empties like a single tank — total headroom over the current rate —
 * and never as a sum of per-account projections. That distinction is the whole
 * number: summing `headroom / own historical rate` scores an untouched spare
 * account at zero, when idle spares are the most valuable thing the user owns.
 *
 * Unknown is never zero here. A missing sample means "we did not measure",
 * and a 0 would read as "confirmed idle" — the same discipline
 * `BurnRateChart`'s `data` doc comment describes for days with no reading.
 *
 * Nothing in this module reads the clock; `now` is always a parameter, as in
 * `src/lib/time.ts`. A projection you cannot pin to a fixed instant is a
 * projection you cannot test.
 */

import { formatCountdown } from "@/lib/time";
import { headroom, type Account, type Sample } from "@/types";

/**
 * How far back a burn rate is measured. Long enough to survive the gaps
 * between polls, short enough that an idle morning does not flatten an active
 * afternoon — the slope is meant to answer "what is happening now".
 */
const BURN_WINDOW_SECONDS = 3 * 3_600;

/** Older than this and any projection built on the reading must be labelled. */
const STALE_AFTER_SECONDS = 600;

/**
 * Statuses that take an account out of the pool entirely. None of these can
 * serve a request, so their headroom is not spendable and counting it would
 * inflate the headline.
 */
const UNUSABLE_STATUSES: ReadonlySet<string> = new Set([
  "disabled",
  "reloginrequired",
  "foreigncredential",
]);

/** `Date` cannot represent an instant beyond this many milliseconds. */
const MAX_DATE_MS = 8.64e15;

/**
 * Past this, the projection stops meaning anything and is reported as a floor.
 *
 * Two reasons, and either alone would justify it. Quota replenishes: the 7-day
 * window resets inside this horizon, so a figure reaching past it is describing
 * a tank that refills before it drains. And the arithmetic is headroom over
 * rate, so a near-flat slope divides by almost nothing — 0.01 points/hour of
 * measurement noise across four accounts yields "1583d 8h", which is precision
 * the reading cannot support.
 */
const RUNWAY_HORIZON_SECONDS = 7 * 86_400;

export interface AccountBurn {
  /** Utilisation points consumed per hour, or null when it cannot be derived. */
  pctPerHour: number | null;
  /** Seconds until this account reaches 100%, or null when unknowable. */
  secondsToLimit: number | null;
}

export interface RunwayEstimate {
  /** Pooled seconds of usable quota left, or null when it cannot be estimated. */
  seconds: number | null;
  /** Pooled burn, utilisation points per hour. Null when unknown. */
  pctPerHour: number | null;
  /** True when any contributing account's reading is stale or unknown. */
  degraded: boolean;
  /** Accounts that actually contributed. Callers show this so the number is auditable. */
  contributing: number;
}

/** Epoch millis, or null when the timestamp is not a usable instant. */
function parseTimestamp(iso: string | undefined): number | null {
  if (!iso) return null;
  const ms = Date.parse(iso);
  return Number.isNaN(ms) ? null : ms;
}

interface Reading {
  at: number;
  pct: number;
}

/** The recent, usable readings, oldest first. Order is established, never assumed. */
function recentReadings(samples: Sample[], now: number): Reading[] {
  const earliest = now - BURN_WINDOW_SECONDS * 1_000;
  return samples
    .map((s) => ({ at: parseTimestamp(s.timestamp), pct: s.bindingPct }))
    .filter(
      (r): r is Reading =>
        r.at !== null && Number.isFinite(r.pct) && r.at <= now && r.at >= earliest,
    )
    .sort((a, b) => a.at - b.at);
}

/**
 * Slope of `bindingPct` across the recent window, and the runway it implies.
 *
 * Measured endpoint-to-endpoint rather than by a fitted line: a window that
 * reset mid-flight shows a fall, and a fit would smooth that into a confident
 * positive rate the account never actually sustained.
 */
export function accountBurn(samples: Sample[], now: number): AccountBurn {
  const readings = recentReadings(samples, now);
  const first = readings[0];
  const last = readings[readings.length - 1];

  // One reading is a position, not a rate.
  if (!first || !last || readings.length < 2) return { pctPerHour: null, secondsToLimit: null };

  const hours = (last.at - first.at) / 3_600_000;
  if (hours <= 0) return { pctPerHour: null, secondsToLimit: null };

  const pctPerHour = (last.pct - first.pct) / hours;
  const headroomPct = 100 - last.pct;

  // Already at the limit: nothing left to spend, whatever the slope says. That
  // is measured rather than unknown, so 0 is the honest answer here.
  if (headroomPct <= 0) {
    return { pctPerHour: pctPerHour > 0 ? pctPerHour : null, secondsToLimit: 0 };
  }

  // Flat or falling means "not burning right now". Dividing by it would claim
  // an infinite runway, so the rate is unknown — never zero.
  if (!(pctPerHour > 0)) return { pctPerHour: null, secondsToLimit: null };

  return { pctPerHour, secondsToLimit: (headroomPct / pctPerHour) * 3_600 };
}

/** This account's current burn, or null when it is not measurably burning. */
function rateOf(
  account: Account,
  samplesByKey: Map<string, Sample[]>,
  keyFor: (a: Account) => string | undefined,
  now: number,
): number | null {
  const key = keyFor(account);
  const samples = (key === undefined ? undefined : samplesByKey.get(key)) ?? [];
  return accountBurn(samples, now).pctPerHour;
}

/** The fastest burn among these accounts, or null when none of them is burning. */
function fastestRate(
  accounts: Account[],
  samplesByKey: Map<string, Sample[]>,
  keyFor: (a: Account) => string | undefined,
  now: number,
): number | null {
  let fastest: number | null = null;
  for (const account of accounts) {
    const rate = rateOf(account, samplesByKey, keyFor, now);
    if (rate !== null && (fastest === null || rate > fastest)) fastest = rate;
  }
  return fastest;
}

/**
 * Runway across every account the user can still switch to: total headroom
 * divided by the rate it is actually being spent at.
 *
 * The rate belongs to the person, not to the slot — one worker, one speed —
 * so it is read from the live account rather than summed across the pool, and
 * an idle spare adds its full headroom rather than scoring zero for having no
 * measurable burn of its own.
 */
export function pooledRunway(
  accounts: Account[],
  samplesByKey: Map<string, Sample[]>,
  keyFor: (a: Account) => string | undefined,
  now: number,
): RunwayEstimate {
  // Exclusions govern which headroom is spendable; they do not govern the
  // rate. An account held out of rotation can still be the installed
  // credential, and its burn is evidence of how fast the user is working.
  const live = accounts.filter((a) => a.active);
  const activeRate = fastestRate(live, samplesByKey, keyFor, now);
  // Live account flat or unmeasurable: borrow the fastest burn seen anywhere.
  // The user is burning something somewhere, and overstating the rate
  // understates the runway — the right direction to be wrong in.
  const rate = activeRate ?? fastestRate(accounts, samplesByKey, keyFor, now);

  // A rate borrowed from an account other than the one being spent is a
  // substitution, and substitutions get labelled rather than hidden.
  let degraded = activeRate === null && rate !== null;
  let headroomPct = 0;
  let contributing = 0;

  for (const account of accounts) {
    // Unusable accounts are not a gap in the estimate; they are outside it.
    if (UNUSABLE_STATUSES.has(account.usageStatus)) continue;

    const remaining = headroom(account.usage);
    if (remaining === null) {
      // Usable but unmeasured — no usage at all, or none this build can read.
      // Its headroom is missing from the pool, so the headline is an
      // undercount and has to say so.
      degraded = true;
      continue;
    }

    // An exhausted account still belongs in the pool, at 0. That is measured,
    // not unknown, and dropping it would quietly shrink `contributing`.
    headroomPct += remaining;
    contributing += 1;

    // A missing age is an unknown age, not a fresh one.
    const age = account.usageAgeSeconds ?? Number.POSITIVE_INFINITY;
    if (account.usageStatus !== "ok" || age > STALE_AFTER_SECONDS) degraded = true;
  }

  return {
    // No rate anywhere means genuinely idle, and nothing measured means
    // nothing to divide — both are unknown, neither is "no runway left".
    seconds: rate !== null && contributing > 0 ? (headroomPct / rate) * 3_600 : null,
    pctPerHour: rate,
    degraded,
    contributing,
  };
}

/**
 * `"2d 3h"` / `"6h 20m"` / `"18m"`, `"now"` once the pool is spent, `"> 7d"`
 * beyond the useful horizon, and `"unknown"` for null.
 *
 * The buckets are not restated here: the duration is anchored at the epoch and
 * handed to `formatCountdown`, which mirrors `oauth.rs::reset_strings`. Two
 * formatters for the same shape of duration is exactly how they drift apart.
 */
export function formatRunway(seconds: number | null): string {
  if (seconds === null || !Number.isFinite(seconds)) return "unknown";
  // A floor, not a measurement — see RUNWAY_HORIZON_SECONDS.
  if (seconds > RUNWAY_HORIZON_SECONDS) return "> 7d";
  // Clamped only so a near-flat slope cannot hand `Date` an unrepresentable
  // instant and throw inside the Dashboard's headline figure.
  const ms = Math.min(Math.max(seconds, 0) * 1_000, MAX_DATE_MS);
  return formatCountdown(new Date(ms).toISOString(), 0) ?? "unknown";
}
