import { useClockFormat } from "../../lib/clockFormat";
import { formatClock } from "../../lib/time";
import type { Account, DayStat, Sample, ScopedSample } from "../../types";
import { bindingUtilisation, displayName, quotaState } from "../../types";
import { Loading } from "../Loading";
import UsageMeter from "../UsageMeter";
import UsageHeatmap from "./UsageHeatmap";

interface AccountAnalyticsProps {
  account: Account;
  /** Intraday samples, ascending, covering the last N hours. May be empty. */
  samples: Sample[];
  /** Daily rollups over `rangeDays`. A day with no measurement simply has no entry. */
  daily: DayStat[];
  rangeDays: number;
  /** Utilisation that triggers an auto-switch; drawn as a dashed hairline. */
  threshold: number;
  loading: boolean;
  onBack: () => void;
}

const VIEW_W = 300;
const WINDOW_H = 84;
/** Matches `.range-chart svg`'s height, so the range chart is drawn 1:1. */
const RANGE_H = 96;
const PAD = 4;

/** One absence, one sentence — the wording HistoryScreen already uses. */
const NO_HISTORY = "No history yet for this account — usage is recorded from now on.";

function yFor(pct: number, height: number): number {
  const clamped = Math.max(0, Math.min(100, pct));
  return height - PAD - (clamped / 100) * (height - PAD * 2);
}

/** `x` arrives as a 0..1 position across the sampled span. */
function xFor(x: number): number {
  return PAD + x * (VIEW_W - PAD * 2);
}

function stateClassOf(pct: number | null): string {
  const state = quotaState(pct);
  return state === "ok" ? "" : ` ${state}`;
}

interface Pt {
  x: number;
  v: number;
}

/**
 * Contiguous runs of readings, positioned by *time* rather than by index and
 * broken wherever the series stops being continuous.
 *
 * Two things break a run, for the same reason: a `null` reading, and a gap far
 * longer than this account's usual polling interval (the app was closed).
 * Both are unknown, not idle, and drawing through either would invent a slope
 * nobody measured — see `BurnRateChart`'s `data` doc comment. Index spacing
 * would tell the same lie more quietly, by drawing an overnight gap the same
 * width as the five minutes either side of it.
 */
function runsOf(samples: Sample[], pick: (s: Sample) => number | null): Pt[][] {
  const pts = samples
    .map((s) => ({ t: Date.parse(s.timestamp), v: pick(s) }))
    .filter((p) => Number.isFinite(p.t));
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
    const v = p.v != null && Number.isFinite(p.v) ? p.v : null;
    if ((v === null || (prev && p.t - prev.t > limit)) && current.length) {
      runs.push(current);
      current = [];
    }
    if (v !== null) current.push({ x: (p.t - t0) / span, v });
  });
  if (current.length) runs.push(current);
  return runs;
}

function linePath(runs: Pt[][]): string {
  return runs
    .map((run) => {
      const d = run
        .map((p, i) => `${i === 0 ? "M" : "L"}${xFor(p.x).toFixed(1)} ${yFor(p.v, WINDOW_H).toFixed(1)}`)
        .join(" ");
      // A lone reading is still a measurement: repeat the point so the round
      // linecap renders a dot rather than nothing at all.
      const only = run[0];
      return run.length === 1 && only ? `${d} L${xFor(only.x).toFixed(1)} ${yFor(only.v, WINDOW_H).toFixed(1)}` : d;
    })
    .join(" ");
}

function areaPath(runs: Pt[][]): string {
  const base = (WINDOW_H - PAD).toFixed(1);
  return runs
    .map((run) => {
      const first = run[0];
      const last = run[run.length - 1];
      if (!first || !last) return "";
      const line = run
        .map((p, i) => `${i === 0 ? "M" : "L"}${xFor(p.x).toFixed(1)} ${yFor(p.v, WINDOW_H).toFixed(1)}`)
        .join(" ");
      return `${line} L${xFor(last.x).toFixed(1)} ${base} L${xFor(first.x).toFixed(1)} ${base} Z`;
    })
    .join(" ");
}

interface WindowChartProps {
  label: string;
  samples: Sample[];
  pick: (s: Sample) => number | null;
  threshold: number;
  startLabel: string;
  endLabel: string;
}

/**
 * One rate-limit window traced across the intraday samples.
 *
 * Separate from `BurnRateChart` on purpose: that is a per-day small multiple
 * for comparing accounts, this is a within-day trace for comparing *windows*
 * of one account. Different axis, and a different head — the peak matters as
 * much as the current value here, because a 5-hour spike has already drained
 * away by the time anyone opens this screen.
 */
function WindowChart({ label, samples, pick, threshold, startLabel, endLabel }: WindowChartProps) {
  const runs = runsOf(samples, pick);
  const values = runs.flatMap((run) => run.map((p) => p.v));
  const peak = values.length ? Math.max(...values) : null;
  const lastRun = runs[runs.length - 1];
  const last = lastRun?.[lastRun.length - 1] ?? null;
  const now = last?.v ?? null;
  const cls = stateClassOf(now);
  const ty = yFor(threshold, WINDOW_H);

  const title =
    peak == null
      ? `${label}: nothing measured in this period`
      : `${label}: peaked at ${Math.round(peak)}%, currently ${Math.round(now ?? peak)}%, ` +
        `auto-switch threshold ${threshold}%`;

  return (
    <div className="chart">
      <div className="chart-head">
        <span className="t">{label}</span>
        <span className="v num">
          {peak == null ? "no data" : `peak ${Math.round(peak)}% · now ${now == null ? "··" : `${Math.round(now)}%`}`}
        </span>
      </div>
      <svg viewBox={`0 0 ${VIEW_W} ${WINDOW_H}`} role="img" aria-label={title}>
        <title>{title}</title>
        <line x1={PAD} x2={VIEW_W - PAD} y1={ty} y2={ty} className="thresh-line" />
        <path d={areaPath(runs)} className={`chart-area${cls}`} />
        <path d={linePath(runs)} className={`chart-line${cls}`} fill="none" />
        {last && <circle cx={xFor(last.x)} cy={yFor(last.v, WINDOW_H)} r={2.5} className={`chart-dot${cls}`} />}
      </svg>
      <div className="xax">
        <span>{startLabel}</span>
        <span>{endLabel}</span>
      </div>
    </div>
  );
}

/**
 * One slot per day across the trailing `days` days, oldest first. A day the
 * backend has no rollup for stays `null`: the app was not running, which is
 * unknown, and never a measured zero.
 */
function dailySlots(daily: DayStat[], days: number): Array<DayStat | null> {
  const byDay = new Map(daily.map((d) => [d.day, d]));
  const today = new Date();
  const out: Array<DayStat | null> = [];
  for (let offset = days - 1; offset >= 0; offset--) {
    const d = new Date(Date.UTC(today.getUTCFullYear(), today.getUTCMonth(), today.getUTCDate()));
    d.setUTCDate(d.getUTCDate() - offset);
    out.push(byDay.get(d.toISOString().slice(0, 10)) ?? null);
  }
  return out;
}

/**
 * Each day's min→max as a band, with the average tracked through them.
 *
 * The History screen plots `avgPct` alone and discards the other two fields,
 * which flattens a day that swung from 10% to 100% into a calm 55%. The spread
 * is the story; the average only says where the day mostly sat. Unmeasured
 * days break the average line rather than joining across it.
 */
function RangeChart({ daily, rangeDays, threshold }: { daily: DayStat[]; rangeDays: number; threshold: number }) {
  const slots = dailySlots(daily, rangeDays);
  const step = (VIEW_W - PAD * 2) / Math.max(slots.length - 1, 1);
  const width = Math.max(Math.min(step * 0.62, 9), 1.5);
  const measured = slots.filter(Boolean).length;
  const ty = yFor(threshold, RANGE_H);

  // Contiguous runs again, for the same reason as the window charts.
  const runs: Array<Array<{ x: number; y: number }>> = [];
  let current: Array<{ x: number; y: number }> = [];
  slots.forEach((stat, i) => {
    if (!stat) {
      if (current.length) runs.push(current);
      current = [];
      return;
    }
    current.push({ x: PAD + i * step, y: yFor(stat.avgPct, RANGE_H) });
  });
  if (current.length) runs.push(current);
  const avgPath = runs
    .map((run) => {
      const d = run.map((p, i) => `${i === 0 ? "M" : "L"}${p.x.toFixed(1)} ${p.y.toFixed(1)}`).join(" ");
      const only = run[0];
      return run.length === 1 && only ? `${d} L${only.x.toFixed(1)} ${only.y.toFixed(1)}` : d;
    })
    .join(" ");

  const title =
    `Daily range over ${rangeDays} days: ${measured} day${measured === 1 ? "" : "s"} measured, ` +
    "each drawn from its lowest to its highest reading with the average tracked through them";

  return (
    <div className="chart range-chart">
      <div className="chart-head">
        <span className="t">Daily range</span>
        <span className="v num">
          {measured} of {rangeDays} days measured
        </span>
      </div>
      <svg viewBox={`0 0 ${VIEW_W} ${RANGE_H}`} role="img" aria-label={title}>
        <title>{title}</title>
        <line x1={PAD} x2={VIEW_W - PAD} y1={ty} y2={ty} className="thresh-line" />
        {slots.map((stat, i) => {
          if (!stat) return null;
          const top = yFor(stat.maxPct, RANGE_H);
          // A day that never moved still deserves a mark, so a flat range keeps
          // a hair of height rather than collapsing to an invisible zero.
          const height = Math.max(yFor(stat.minPct, RANGE_H) - top, 1);
          return (
            <rect
              key={stat.day}
              // Clamped so the first and last bands sit inside the viewBox
              // rather than being half-clipped by it.
              x={Math.min(Math.max(PAD + i * step - width / 2, 0), VIEW_W - width)}
              y={top}
              width={width}
              height={height}
              rx={0.8}
              className="range-band"
            >
              <title>
                {`${stat.day}: ${Math.round(stat.minPct)}–${Math.round(stat.maxPct)}%, average ` +
                  `${Math.round(stat.avgPct)}% over ${stat.sampleCount} sample${stat.sampleCount === 1 ? "" : "s"}`}
              </title>
            </rect>
          );
        })}
        <path d={avgPath} className="range-avg" strokeLinecap="round" strokeLinejoin="round" />
      </svg>
      <div className="xax">
        <span>{rangeDays} days ago</span>
        <span>today</span>
      </div>
    </div>
  );
}

/** The account's headline state in words — hue alone never carries it. */
function statusLabel(account: Account): string {
  switch (account.usageStatus) {
    case "disabled":
      return "held out";
    case "reloginrequired":
    case "expired":
      return "re-login required";
    case "foreigncredential":
      return "credential mismatch";
    case "stale":
      return "last known";
    case "unavailable":
      return "usage unavailable";
    case "error":
      return "error";
    case "unknown":
      return "never measured";
    default:
      return account.active ? "active" : "idle";
  }
}

/**
 * One account, opened from the dashboard's fleet list.
 *
 * The screen exists for its first section: the 5-hour and 7-day windows drawn
 * apart. Every other view collapses them into one figure — the History screen
 * into a single daily average — which erases exactly the short spike that
 * makes the daemon switch accounts.
 */
export default function AccountAnalytics({
  account,
  samples,
  daily,
  rangeDays,
  threshold,
  loading,
  onBack,
}: AccountAnalyticsProps) {
  const clockFormat = useClockFormat();
  const binding = bindingUtilisation(account.usage);
  const bindingClass = quotaState(binding) === "ok" ? "" : ` ${quotaState(binding)}`;

  const firstAt = samples[0]?.timestamp;
  const lastAt = samples[samples.length - 1]?.timestamp;
  const startLabel = formatClock(firstAt, clockFormat) ?? "earlier";
  const endLabel = formatClock(lastAt, clockFormat) ?? "now";

  // The back control and the heading stay mounted through every state below:
  // a view that loses its way out while loading is a trap.
  const head = (
    <div className="analytics-head">
      <button type="button" className="btn-back" onClick={onBack}>
        ← All accounts
      </button>
      <h3>{displayName(account)}</h3>
      <span className={`pill${account.active ? " on" : ""}`}>{statusLabel(account)}</span>
      <span className={`pill num${bindingClass}`}>
        {binding == null ? "no reading" : `${Math.round(binding)}% used`}
      </span>
    </div>
  );

  if (loading) {
    return (
      <div className="analytics">
        {head}
        <Loading label="Loading this account's history" />
      </div>
    );
  }

  const hasSamples = samples.length > 0;
  const hasDaily = daily.length > 0;

  if (!hasSamples && !hasDaily) {
    return (
      <div className="analytics">
        {head}
        <div className="empty">
          <h3>No history yet</h3>
          <p>{NO_HISTORY}</p>
        </div>
      </div>
    );
  }

  // Per-model windows come from the newest sample rather than `account.usage`
  // when there is one, so every figure on this screen was read at one instant.
  const latest = samples[samples.length - 1];
  const models: ScopedSample[] = latest?.scoped ?? account.usage?.scoped ?? [];

  return (
    <div className="analytics">
      {head}

      {/* Caption class on an h3: only h1–h3 carry app.css's margin reset, and
          this flex column depends on zero-margin children. */}
      <h3 className="dash-cap">The two windows, apart</h3>
      {hasSamples ? (
        <div className="split-grid">
          <WindowChart
            label="5-hour window"
            samples={samples}
            pick={(s) => s.fiveHourPct}
            threshold={threshold}
            startLabel={startLabel}
            endLabel={endLabel}
          />
          <WindowChart
            label="7-day window"
            samples={samples}
            pick={(s) => s.sevenDayPct}
            threshold={threshold}
            startLabel={startLabel}
            endLabel={endLabel}
          />
        </div>
      ) : (
        <div className="empty">
          <p>{NO_HISTORY}</p>
        </div>
      )}

      <h3 className="dash-cap">Per-model weekly windows</h3>
      {models.length === 0 ? (
        <span className="dash-cap">None reported.</span>
      ) : (
        <div className="model-rows">
          {/* Row layout reuses the details panel's primitive rather than a
              second one that would only drift from it. */}
          {models.map((m) => (
            <div className="acct-details-model-row" key={m.name}>
              <span className="model-name">{m.name}</span>
              <UsageMeter pct={m.pct} />
            </div>
          ))}
        </div>
      )}

      <h3 className="dash-cap">When this account runs hot</h3>
      <UsageHeatmap samples={samples} />

      <h3 className="dash-cap">Daily range</h3>
      {hasDaily ? (
        <RangeChart daily={daily} rangeDays={rangeDays} threshold={threshold} />
      ) : (
        <div className="empty">
          <p>{NO_HISTORY}</p>
        </div>
      )}
    </div>
  );
}
