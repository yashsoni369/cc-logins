import { quotaState } from "../types";

/** One account's burn-rate samples for a single history range. */
export interface BurnRateSeries {
  id: string;
  label: string;
  /**
   * Utilisation, 0..100, oldest first, one slot per day. `null` marks a day
   * with no recorded measurement — a real gap (e.g. the app wasn't running),
   * never coerced to 0, because a 0 there would read as "confirmed idle"
   * rather than "unknown". The chart draws a gap in the line rather than
   * dropping to the baseline.
   */
  data: Array<number | null>;
}

interface BurnRateChartProps {
  series: BurnRateSeries;
  /** Utilisation that triggers an auto-switch; drawn as a dashed hairline. */
  threshold?: number;
  /** Left x-axis label, e.g. "30 days ago". */
  startLabel: string;
  /** Right x-axis label. */
  endLabel?: string;
}

const VIEW_W = 300;
const VIEW_H = 84;
const PAD = 4;

function yFor(pct: number): number {
  return VIEW_H - PAD - (pct / 100) * (VIEW_H - PAD * 2);
}

function xStep(count: number): number {
  return (VIEW_W - PAD * 2) / Math.max(count - 1, 1);
}

/** Contiguous runs of non-null indices — each run is drawn as its own path segment. */
function contiguousRuns(data: Array<number | null>): Array<Array<{ i: number; v: number }>> {
  const runs: Array<Array<{ i: number; v: number }>> = [];
  let current: Array<{ i: number; v: number }> = [];
  data.forEach((v, i) => {
    if (v == null) {
      if (current.length) runs.push(current);
      current = [];
    } else {
      current.push({ i, v });
    }
  });
  if (current.length) runs.push(current);
  return runs;
}

/**
 * Builds an SVG path from a data array — never hand-authored. A `null` entry
 * breaks the line rather than being treated as 0: a day with no measurement
 * is unknown, not confirmed-idle, and drawing through it as a straight line
 * to the next point would fabricate a trend that was never observed.
 */
function buildLinePath(data: Array<number | null>): string {
  const step = xStep(data.length);
  return contiguousRuns(data)
    .map((run) =>
      run
        .map(({ i, v }, j) => `${j === 0 ? "M" : "L"}${(PAD + i * step).toFixed(1)} ${yFor(v).toFixed(1)}`)
        .join(" "),
    )
    .join(" ");
}

/** Same line, closed down to the baseline per contiguous run, for the faint area fill. */
function buildAreaPath(data: Array<number | null>): string {
  const step = xStep(data.length);
  const base = (VIEW_H - PAD).toFixed(1);
  return contiguousRuns(data)
    .map((run) => {
      const line = run
        .map(({ i, v }, j) => `${j === 0 ? "M" : "L"}${(PAD + i * step).toFixed(1)} ${yFor(v).toFixed(1)}`)
        .join(" ");
      const firstIdx = run[0];
      const lastIdx = run[run.length - 1];
      if (!firstIdx || !lastIdx) return "";
      const right = (PAD + lastIdx.i * step).toFixed(1);
      const left = (PAD + firstIdx.i * step).toFixed(1);
      return `${line} L${right} ${base} L${left} ${base} Z`;
    })
    .join(" ");
}

/** Last index in `data` carrying a real (non-null) value, if any. */
function lastKnownIndex(data: Array<number | null>): number {
  for (let i = data.length - 1; i >= 0; i--) {
    if (data[i] != null) return i;
  }
  return -1;
}

/**
 * Small-multiple burn-rate chart: one account, one shared axis. History is
 * meant to compare accounts, so this is deliberately never combined into a
 * single multi-line chart — see HistoryScreen, which renders one of these
 * per account on identical scales.
 */
export default function BurnRateChart({ series, threshold = 90, startLabel, endLabel = "today" }: BurnRateChartProps) {
  const { label, data } = series;
  const knownIndex = lastKnownIndex(data);
  const last = knownIndex === -1 ? null : (data[knownIndex] ?? null);
  const state = quotaState(last);
  const stateClass = state === "ok" ? "" : ` ${state}`;
  const crossed = data.some((v) => v != null && v >= threshold);
  const step = xStep(data.length);
  const cx = PAD + knownIndex * step;
  const cy = last == null ? 0 : yFor(last);
  const ty = yFor(threshold);
  const lastLabel = last == null ? null : `${Math.round(last)}%`;

  const title = `${label}: ${startLabel} to ${endLabel}, currently ${lastLabel ?? "no data"}${
    crossed ? ", reached the auto-switch threshold this period" : ""
  }`;

  return (
    <div className="chart">
      <div className="chart-head">
        <span className="t">{label}</span>
        <span className="v num">
          {lastLabel == null ? "no data" : `now ${lastLabel}`}
          {crossed ? " · hit limit" : ""}
        </span>
      </div>
      <svg viewBox={`0 0 ${VIEW_W} ${VIEW_H}`} role="img" aria-label={title}>
        <title>{title}</title>
        <line x1={PAD} x2={VIEW_W - PAD} y1={ty} y2={ty} className="thresh-line" />
        <path d={buildAreaPath(data)} className={`chart-area${stateClass}`} />
        <path d={buildLinePath(data)} className={`chart-line${stateClass}`} fill="none" />
        {last != null && <circle cx={cx} cy={cy} r={2.5} className={`chart-dot${stateClass}`} />}
      </svg>
      <div className="xax">
        <span>{startLabel}</span>
        <span>{endLabel}</span>
      </div>
    </div>
  );
}
