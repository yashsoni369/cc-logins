/**
 * The dashboard's chart primitives.
 *
 * Extracted from the old full-screen account view so the in-place drawer and
 * anything else render the same marks from the same code. Gap handling lives
 * in `lib/dashboard.ts` and is shared with the fleet chart — a run broken in
 * one place and joined in another would be the same data telling two stories.
 */

import { runsOf, type Pt } from "@/lib/dashboard";
import { quotaState, type DayStat, type Sample } from "@/types";

const VIEW_W = 300;
const WINDOW_H = 84;
/** Matches `.range-chart svg`'s height, so the range chart is drawn 1:1. */
const RANGE_H = 96;
const PAD = 4;

export function yFor(pct: number, height: number): number {
  const clamped = Math.max(0, Math.min(100, pct));
  return height - PAD - (clamped / 100) * (height - PAD * 2);
}

/** `x` arrives as a 0..1 position across the sampled span. */
export function xFor(x: number): number {
  return PAD + x * (VIEW_W - PAD * 2);
}

function stateClassOf(pct: number | null): string {
  const state = quotaState(pct);
  return state === "ok" ? "" : ` ${state}`;
}

function linePath(runs: Pt[][], height: number): string {
  return runs
    .map((run) => {
      const d = run
        .map((p, i) => `${i === 0 ? "M" : "L"}${xFor(p.x).toFixed(1)} ${yFor(p.v, height).toFixed(1)}`)
        .join(" ");
      // A lone reading is still a measurement: repeat the point so the round
      // linecap renders a dot rather than nothing at all.
      const only = run[0];
      return run.length === 1 && only
        ? `${d} L${xFor(only.x).toFixed(1)} ${yFor(only.v, height).toFixed(1)}`
        : d;
    })
    .join(" ");
}

function areaPath(runs: Pt[][], height: number): string {
  const base = (height - PAD).toFixed(1);
  return runs
    .map((run) => {
      const first = run[0];
      const last = run[run.length - 1];
      if (!first || !last) return "";
      const line = run
        .map((p, i) => `${i === 0 ? "M" : "L"}${xFor(p.x).toFixed(1)} ${yFor(p.v, height).toFixed(1)}`)
        .join(" ");
      return `${line} L${xFor(last.x).toFixed(1)} ${base} L${xFor(first.x).toFixed(1)} ${base} Z`;
    })
    .join(" ");
}

/**
 * The trend behind a fleet row, drawn from the series the rotation chart
 * already built — a row that shows only a percentage says where an account is
 * but not which way it is going, and the direction is what decides whether to
 * switch to it.
 */
export function Sparkline({ runs, last, label }: { runs: Pt[][]; last: number | null; label: string }) {
  const W = 120;
  const H = 20;
  const P = 2;
  const x = (v: number) => P + v * (W - P * 2);
  const y = (v: number) => H - P - (Math.max(0, Math.min(100, v)) / 100) * (H - P * 2);

  if (runs.length === 0 || last == null) {
    return (
      <span className="row-spark is-empty" title={`No usage recorded for ${label} in this range`}>
        <span className="pct-unknown">··</span>
      </span>
    );
  }

  const state = quotaState(last);
  const cls = state === "ok" ? "" : ` ${state}`;
  const lastRun = runs[runs.length - 1];
  const tip = lastRun?.[lastRun.length - 1];

  return (
    <span className="row-spark">
      {/* `none` is right here and wrong on the big chart: there is no text in
          a sparkline to distort, and uniform scaling letterboxed a 6:1 drawing
          inside a much wider cell, stranding it in the middle of the row. */}
      <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" role="img"
        aria-label={`${label}: utilisation trend, now ${Math.round(last)}%`}>
        {runs.map((run, i) => (
          <path
            key={i}
            d={run.map((p, k) => `${k === 0 ? "M" : "L"}${x(p.x).toFixed(1)} ${y(p.v).toFixed(1)}`).join("")}
            className={`chart-line${cls}`}
            fill="none"
          />
        ))}
        {tip && <circle cx={x(tip.x)} cy={y(tip.v)} r={1.8} className={`chart-dot${cls}`} />}
      </svg>
    </span>
  );
}

interface WindowChartProps {
  label: string;
  samples: Sample[];
  pick: (s: Sample) => number | null;
  /** Utilisation that triggers an auto-switch; drawn as a dashed hairline. */
  threshold: number;
  startLabel: string;
  endLabel: string;
}

/**
 * One rate-limit window traced across the intraday samples.
 *
 * The head reports the peak as well as the current value, because a 5-hour
 * spike has usually drained away by the time anyone opens this — and the
 * spike is what made the daemon switch.
 */
export function WindowChart({ label, samples, pick, threshold, startLabel, endLabel }: WindowChartProps) {
  const runs = runsOf(samples, (s) => Date.parse(s.timestamp), pick);
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
        <path d={areaPath(runs, WINDOW_H)} className={`chart-area${cls}`} />
        <path d={linePath(runs, WINDOW_H)} className={`chart-line${cls}`} fill="none" />
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
 * backend has no rollup for stays null: the app was not running, which is
 * unknown, and never a measured zero.
 */
export function dailySlots(daily: DayStat[], days: number, now = Date.now()): Array<DayStat | null> {
  const byDay = new Map(daily.map((d) => [d.day, d]));
  const today = new Date(now);
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
 * Plotting the average alone flattens a day that swung from 10% to 100% into
 * a calm 55%. The spread is the story. Unmeasured days break the average line
 * rather than joining across it.
 */
export function RangeChart({
  daily,
  rangeDays,
  threshold,
}: {
  daily: DayStat[];
  rangeDays: number;
  threshold: number;
}) {
  const slots = dailySlots(daily, rangeDays);
  const step = (VIEW_W - PAD * 2) / Math.max(slots.length - 1, 1);
  const width = Math.max(Math.min(step * 0.62, 9), 1.5);
  const measured = slots.filter(Boolean).length;
  const ty = yFor(threshold, RANGE_H);

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
