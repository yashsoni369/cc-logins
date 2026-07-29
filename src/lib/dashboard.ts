/**
 * Everything the dashboard derives, with no React in it.
 *
 * The screen asks four questions — how long does the fleet last, who is
 * carrying it, when does relief arrive, and is the rotation healthy. Every
 * answer here is computed from history the app already records; nothing in
 * this module invents a figure it was not given, and each derivation returns
 * an explicit "not enough data" rather than a confident zero.
 */

import type { Account, DayStat, Sample } from "@/types";
import { bindingUtilisation, displayName } from "@/types";

// ── ranges ───────────────────────────────────────────────────────────────────

export type RangeKey = "24h" | "7d" | "30d" | "all";

export interface RangeSpec {
  key: RangeKey;
  /** Control label. */
  label: string;
  /** Prose for band subtitles, e.g. "last 7 days". */
  phrase: string;
  /**
   * Which read path backs the rotation chart.
   *
   * `HistoryStore::prune` compacts samples older than the retention window
   * into daily rollups and deletes them, so a long range asked for in samples
   * would come back progressively emptier the further back it reached. Daily
   * rollups are the only source that spans the whole history.
   */
  source: "samples" | "daily";
  /** Trailing hours to request when `source` is "samples". */
  hours: number;
  /** Trailing days, used for the load-balance grid in every range. */
  days: number;
}

export const RANGES: Record<RangeKey, RangeSpec> = {
  "24h": { key: "24h", label: "24h", phrase: "last 24 hours", source: "samples", hours: 24, days: 1 },
  "7d": { key: "7d", label: "7 days", phrase: "last 7 days", source: "samples", hours: 168, days: 7 },
  "30d": { key: "30d", label: "30 days", phrase: "last 30 days", source: "daily", hours: 720, days: 30 },
  // Retention defaults to 90 days; asking for a year simply returns whatever
  // exists rather than pretending to a fixed span.
  all: { key: "all", label: "All", phrase: "all recorded history", source: "daily", hours: 8760, days: 365 },
};

export const RANGE_ORDER: RangeKey[] = ["24h", "7d", "30d", "all"];

export function isRangeKey(value: string): value is RangeKey {
  return value in RANGES;
}

// ── series ───────────────────────────────────────────────────────────────────

/** A point positioned by time, `x` normalised to 0..1 across the series span. */
export interface Pt {
  x: number;
  v: number;
}

/**
 * Contiguous runs of readings, positioned by *time* and broken wherever the
 * series stops being continuous.
 *
 * Two things break a run, for the same reason: a null reading, and a gap far
 * longer than the usual cadence (the app was not running). Both are unknown,
 * not idle, and drawing through either invents a slope nobody measured.
 * Index spacing would tell the same lie more quietly, by drawing an overnight
 * gap the same width as the five minutes either side of it.
 *
 * Generic over the item so samples (ISO instants) and daily rollups (dates)
 * share one implementation and cannot drift apart.
 */
export function runsOf<T>(
  items: T[],
  time: (item: T) => number,
  value: (item: T) => number | null,
): Pt[][] {
  const pts = items
    .map((item) => ({ t: time(item), v: value(item) }))
    .filter((p) => Number.isFinite(p.t))
    .sort((a, b) => a.t - b.t);
  if (pts.length === 0) return [];

  const t0 = pts[0]?.t ?? 0;
  const span = Math.max((pts[pts.length - 1]?.t ?? t0) - t0, 1);

  const gaps = pts
    .slice(1)
    .map((p, i) => p.t - (pts[i]?.t ?? p.t))
    .filter((g) => g > 0)
    .sort((a, b) => a - b);
  const median = gaps.length ? (gaps[Math.floor(gaps.length / 2)] ?? 0) : 0;
  // 3× the usual cadence, and never tighter than a quarter hour: a poll that
  // merely ran late must not shred the line into confetti.
  const limit = Math.max(median * 3, 15 * 60_000);

  const runs: Pt[][] = [];
  let current: Pt[] = [];
  pts.forEach((p, i) => {
    const prev = i > 0 ? pts[i - 1] : undefined;
    const v = p.v != null && Number.isFinite(p.v) ? Math.max(0, Math.min(100, p.v)) : null;
    if ((v === null || (prev && p.t - prev.t > limit)) && current.length) {
      runs.push(current);
      current = [];
    }
    if (v !== null) current.push({ x: (p.t - t0) / span, v });
  });
  if (current.length) runs.push(current);
  return runs;
}

/**
 * Thin a run to at most `max` points without flattening its spikes.
 *
 * Plain every-nth sampling drops peaks, and a quota chart exists to show
 * peaks — the spike that triggers a switch is the whole point. Each bucket
 * therefore contributes its highest and lowest reading, in the order they
 * occurred, which preserves the envelope at any zoom.
 */
export function thin(run: Pt[], max: number): Pt[] {
  if (max < 2) return run.length ? [run[0] as Pt] : [];
  if (run.length <= max) return run;

  const buckets = Math.max(1, Math.floor(max / 2));
  const size = run.length / buckets;
  const out: Pt[] = [];

  for (let b = 0; b < buckets; b++) {
    const start = Math.floor(b * size);
    const end = Math.min(run.length, Math.max(start + 1, Math.floor((b + 1) * size)));
    let lo = run[start] as Pt;
    let hi = lo;
    for (let i = start; i < end; i++) {
      const p = run[i] as Pt;
      if (p.v < lo.v) lo = p;
      if (p.v > hi.v) hi = p;
    }
    const [first, second] = lo.x <= hi.x ? [lo, hi] : [hi, lo];
    out.push(first);
    if (second !== first) out.push(second);
  }
  return out;
}

/** One account's line on the rotation chart. */
export interface FleetSeries {
  accountKey: string;
  number: number;
  name: string;
  /** Broken into runs so unmeasured stretches stay unmeasured. */
  runs: Pt[][];
  /** Newest reading in the series, or null when nothing was measured. */
  last: number | null;
  /** Highest reading across the range. */
  peak: number | null;
  /** Mean of every reading, the basis for "who carried the load". */
  mean: number | null;
  active: boolean;
  heldOut: boolean;
}

const dayMs = (day: string) => Date.parse(`${day}T12:00:00Z`);

/**
 * Build every account's line for the rotation chart.
 *
 * Reads samples or daily rollups depending on the range — see `RangeSpec.source`
 * for why a long range cannot use samples. Accounts with no history still get a
 * series, with empty runs, so the legend does not silently lose a row.
 */
export function buildFleetSeries(
  accounts: Account[],
  keyFor: (a: Account) => string | undefined,
  samplesByKey: Map<string, Sample[]>,
  dailyByKey: Map<string, DayStat[]>,
  spec: RangeSpec,
  maxPoints = 240,
): FleetSeries[] {
  const out: FleetSeries[] = [];

  for (const account of accounts) {
    const accountKey = keyFor(account);
    if (!accountKey) continue;

    const runs =
      spec.source === "samples"
        ? runsOf(
            samplesByKey.get(accountKey) ?? [],
            (s) => Date.parse(s.timestamp),
            (s) => s.bindingPct,
          )
        : runsOf(dailyByKey.get(accountKey) ?? [], (d) => dayMs(d.day), (d) => d.avgPct);

    const thinned = runs.map((run) => thin(run, Math.max(2, Math.floor(maxPoints / Math.max(runs.length, 1)))));
    const values = thinned.flatMap((run) => run.map((p) => p.v));
    const lastRun = thinned[thinned.length - 1];

    out.push({
      accountKey,
      number: account.number,
      name: displayName(account),
      runs: thinned,
      last: lastRun?.[lastRun.length - 1]?.v ?? null,
      peak: values.length ? Math.max(...values) : null,
      mean: values.length ? values.reduce((t, v) => t + v, 0) / values.length : null,
      active: account.active,
      heldOut: account.usageStatus === "disabled",
    });
  }

  return out;
}

// ── pooled headroom ──────────────────────────────────────────────────────────

export interface HeadroomSegment {
  number: number;
  name: string;
  /** Percentage points of this account's own quota still unused, 0..100. */
  free: number;
  /** Excluded from the pooled total — held out, or unreadable. */
  excluded: boolean;
  /** Null when usage could not be read; the segment renders as unknown. */
  binding: number | null;
}

export interface Headroom {
  segments: HeadroomSegment[];
  /** Sum of `free` across usable accounts, in percentage points. */
  pooled: number;
  /** How many accounts contributed, and how many exist. */
  usable: number;
  total: number;
}

/**
 * Remaining capacity per account, and pooled across the fleet.
 *
 * Held-out accounts and accounts with no readable usage are shown but not
 * counted: the pooled figure has to mean "capacity the switcher can actually
 * reach", or it reassures about headroom nothing can spend.
 */
export function pooledHeadroom(accounts: Account[]): Headroom {
  const segments = accounts.map((account) => {
    const binding = bindingUtilisation(account.usage);
    const heldOut = account.usageStatus === "disabled";
    const unreadable = binding == null;
    return {
      number: account.number,
      name: displayName(account),
      free: binding == null ? 0 : Math.max(0, 100 - binding),
      excluded: heldOut || unreadable,
      binding,
    };
  });

  const contributing = segments.filter((s) => !s.excluded);
  return {
    segments,
    pooled: contributing.reduce((total, s) => total + s.free, 0),
    usable: contributing.length,
    total: accounts.length,
  };
}

// ── load balance ─────────────────────────────────────────────────────────────

export interface LoadCell {
  /** `YYYY-MM-DD`, UTC. */
  day: string;
  /** That day's highest reading, or null when the day was never measured. */
  peak: number | null;
  sampleCount: number;
}

export interface LoadRow {
  number: number;
  name: string;
  cells: LoadCell[];
}

/**
 * One row per account, one cell per calendar day, oldest first.
 *
 * A day with no rollup stays null rather than zero: the app was not running,
 * which is unknown, and an unknown day drawn as "idle" would make a gap in
 * recording look like restraint.
 */
export function loadBalanceGrid(
  accounts: Account[],
  keyFor: (a: Account) => string | undefined,
  dailyByKey: Map<string, DayStat[]>,
  days: number,
  now = Date.now(),
): LoadRow[] {
  const span = Math.max(1, Math.floor(days));
  const today = new Date(now);
  const dayKeys: string[] = [];
  for (let offset = span - 1; offset >= 0; offset--) {
    const d = new Date(Date.UTC(today.getUTCFullYear(), today.getUTCMonth(), today.getUTCDate()));
    d.setUTCDate(d.getUTCDate() - offset);
    dayKeys.push(d.toISOString().slice(0, 10));
  }

  return accounts.map((account) => {
    const accountKey = keyFor(account);
    const byDay = new Map((accountKey ? dailyByKey.get(accountKey) : undefined)?.map((d) => [d.day, d]) ?? []);
    return {
      number: account.number,
      name: displayName(account),
      cells: dayKeys.map((day) => {
        const stat = byDay.get(day);
        return { day, peak: stat ? stat.maxPct : null, sampleCount: stat?.sampleCount ?? 0 };
      }),
    };
  });
}

// ── insights ─────────────────────────────────────────────────────────────────

export type InsightTone = "neutral" | "caution" | "danger";

export interface Insight {
  id: string;
  /** The figure, set apart in the headline. */
  figure: string;
  /** Rest of the headline, following the figure. */
  headline: string;
  /** What it means and what to do about it. */
  detail: string;
  tone: InsightTone;
}

/** Accounts whose 5-hour windows reset within this of each other are clustered. */
const CLUSTER_MS = 60 * 60_000;
/** At or above this, a window is treated as spent. */
const SATURATED = 99;
/** Below this across a whole range, an account contributed nothing worth counting. */
const IDLE = 20;

export interface InsightInput {
  accounts: Account[];
  series: FleetSeries[];
  rows: LoadRow[];
  spec: RangeSpec;
  threshold: number;
  now?: number;
}

/**
 * Plain-language findings, in the order a person would care about them.
 *
 * Every one is suppressed unless the data actually supports it — an insight
 * that fires on two readings is worse than no insight, because it reads with
 * the same authority as one drawn from a month. None of these are advice
 * about money or entitlements; they describe measurements already taken.
 */
export function deriveInsights({
  accounts,
  series,
  rows,
  spec,
  threshold,
  now = Date.now(),
}: InsightInput): Insight[] {
  const out: Insight[] = [];
  const measured = series.filter((s) => s.mean != null);

  // 1. Load concentration. Needs at least two measured accounts to be a
  //    comparison at all.
  if (measured.length >= 2) {
    const total = measured.reduce((t, s) => t + (s.mean ?? 0), 0);
    const top = [...measured].sort((a, b) => (b.mean ?? 0) - (a.mean ?? 0))[0];
    if (top && total > 0) {
      const share = Math.round(((top.mean ?? 0) / total) * 100);
      const even = Math.round(100 / measured.length);
      // Only worth saying when it is meaningfully worse than an even split.
      if (share >= even + 15) {
        const idle = measured.filter((s) => s !== top && (s.mean ?? 0) < IDLE).length;
        out.push({
          id: "concentration",
          figure: `${share}%`,
          headline: "of the fleet's load sat on one account",
          detail:
            `${top.name} carried most of the ${spec.phrase}` +
            (idle > 0
              ? `, while ${idle} other${idle === 1 ? "" : "s"} stayed under ${IDLE}% throughout. ` +
                "A lower switch threshold moves work off it sooner."
              : ". A lower switch threshold moves work off it sooner."),
          tone: share >= even + 35 ? "danger" : "caution",
        });
      }
    }
  }

  // 2. Saturation. Counted from daily peaks, which survive pruning — sample
  //    counts would quietly shrink as history ages into rollups.
  const saturatedDays = rows.reduce(
    (total, row) => total + row.cells.filter((c) => c.peak != null && c.peak >= SATURATED).length,
    0,
  );
  if (saturatedDays > 0) {
    out.push({
      id: "saturation",
      figure: String(saturatedDays),
      headline: `day${saturatedDays === 1 ? "" : "s"} an account reached its limit`,
      detail:
        `Each one is a stall you felt. The switcher moves at ${threshold}%; ` +
        "lowering it trades a little unused quota for fewer interruptions.",
      tone: saturatedDays >= 3 ? "danger" : "caution",
    });
  }

  // 3. Reset clustering — read from live usage, so it is about what happens
  //    next rather than what already happened.
  const resets = accounts
    .filter((a) => a.usageStatus !== "disabled")
    .map((a) => Date.parse(a.usage?.fiveHour?.resetsAt ?? ""))
    .filter((ms) => Number.isFinite(ms) && ms > now)
    .sort((a, b) => a - b);
  if (resets.length >= 3) {
    const first = resets[0] as number;
    const clustered = resets.filter((ms) => ms - first <= CLUSTER_MS).length;
    if (clustered >= 3) {
      out.push({
        id: "clustering",
        figure: `${clustered} of ${resets.length}`,
        headline: "accounts free up within an hour of each other",
        detail:
          "Clustered resets give you one large refill and then a long dry stretch. " +
          "Holding one account out for a cycle spreads them apart.",
        tone: "caution",
      });
    }
  }

  // 4. Unused fleet. Only meaningful once there is more than one account.
  const idleAccounts = measured.filter((s) => (s.peak ?? 0) < IDLE && !s.heldOut);
  if (measured.length >= 2 && idleAccounts.length > 0 && idleAccounts.length < measured.length) {
    out.push({
      id: "idle",
      figure: String(idleAccounts.length),
      headline: `account${idleAccounts.length === 1 ? "" : "s"} never went above ${IDLE}%`,
      detail:
        `${idleAccounts.map((s) => s.name).join(", ")} stayed near idle for the ${spec.phrase}. ` +
        "That is capacity the rotation is not reaching.",
      tone: "neutral",
    });
  }

  return out;
}
