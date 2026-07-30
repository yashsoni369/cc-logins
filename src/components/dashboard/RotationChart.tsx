import { useState } from "react";

import type { FleetSeries, RangeSpec } from "@/lib/dashboard";
import { useElementWidth } from "@/lib/useElementWidth";
import { quotaState } from "@/types";

/** Fallback width before the container has been measured. */
const W_FALLBACK = 760;
const H = 190;
const L = 30;
/**
 * Room for the direct end labels, which replace a colour legend.
 *
 * Wide enough for a masked address plus its percentage. Truncating to fit a
 * narrow gutter defeats the purpose — `y•••@gmail…` and `u•••@gmail…` differ
 * only in the first character, so a clipped label identifies nothing.
 */
const R = 132;
const T = 10;
const B = 24;
/** Vertical room one end label needs before it collides with its neighbour. */
const LABEL_PITCH = 11;

const IH = H - T - B;

const yAt = (v: number) => T + IH - (Math.max(0, Math.min(100, v)) / 100) * IH;

const TICKS: Record<string, string[]> = {
  "24h": ["24h ago", "18h", "12h", "6h", "now"],
  "7d": ["7d ago", "5d", "3d", "1d", "now"],
  "30d": ["30d ago", "22d", "15d", "7d", "now"],
  all: ["oldest", "", "", "", "now"],
};

interface RotationChartProps {
  series: FleetSeries[];
  spec: RangeSpec;
  /** Auto-switch threshold, drawn as a dashed hairline. */
  threshold: number;
}

/**
 * Every account's binding utilisation on one pair of axes.
 *
 * Identity is carried by line weight and a direct end label, not by hue.
 * `tokens.css` reserves colour for quota state alone, and honouring that is
 * also the better chart: a per-series palette would make an account at 20%
 * and one at 95% equally loud, when the whole point is spotting the one
 * that is about to run out. Colour appears only on the endpoint dot, where
 * it means what it means everywhere else in the app.
 */
export default function RotationChart({ series, spec, threshold }: RotationChartProps) {
  const [isolated, setIsolated] = useState<number | null>(null);
  // Drawn at one user unit per pixel, so the type stays 9.5px however wide the
  // pane grows. A fixed viewBox scaled to fit would enlarge every label with it.
  const [wrapRef, measured] = useElementWidth<HTMLDivElement>(W_FALLBACK);
  // Matches `.fleetchart`'s own min-width, so that once the wrapper is narrower
  // than the chart and starts scrolling, the viewBox still equals the rendered
  // width and the 1:1 mapping holds.
  const W = Math.max(measured, 480);
  const IW = Math.max(120, W - L - R);
  const xAt = (x: number) => L + x * IW;
  const drawable = series.filter((s) => s.runs.length > 0);

  if (drawable.length === 0) {
    return (
      <div className="empty">
        <p>No usage recorded in the {spec.phrase} — history is written as the app runs.</p>
      </div>
    );
  }

  const ticks = TICKS[spec.key] ?? TICKS["7d"] ?? [];

  /*
   * Resolve label collisions once, for all series together.
   *
   * Nudging each label down as it is drawn pushed anything near 0% clean off
   * the bottom of the plot, and left labels drifting far from the line they
   * name. Sorting by the value first, spreading downward, then pulling the
   * whole stack back up if it overflows keeps every label inside the box and
   * in the same vertical order as the lines themselves.
   */
  const labelled = drawable
    .filter((s) => s.last != null)
    .map((s) => ({ series: s, y: yAt(s.last as number) }))
    .sort((a, b) => a.y - b.y);

  for (let i = 1; i < labelled.length; i++) {
    const prev = labelled[i - 1] as { y: number };
    const cur = labelled[i] as { y: number };
    if (cur.y - prev.y < LABEL_PITCH) cur.y = prev.y + LABEL_PITCH;
  }
  const overflow = (labelled[labelled.length - 1]?.y ?? 0) - (T + IH);
  if (overflow > 0) for (const l of labelled) l.y -= overflow;
  for (const l of labelled) l.y = Math.max(T + 4, l.y);

  const labelYFor = (number: number) => labelled.find((l) => l.series.number === number)?.y ?? 0;

  return (
    <div className="chartwrap" ref={wrapRef}>
      <svg
        className="fleetchart"
        /* The viewBox tracks the measured width, so this is 1:1 with pixels
           and no scaling is applied at all. Stretching a fixed box distorted
           the type; fitting one letterboxed the drawing on a wide pane. */
        viewBox={`0 0 ${W} ${H}`}
        preserveAspectRatio="xMidYMid meet"
        role="img"
        aria-label={`Binding utilisation per account over the ${spec.phrase}`}
        onMouseLeave={() => setIsolated(null)}
      >
        {/* 50 is dropped when the threshold would sit on top of it. */}
        {[0, ...(Math.abs(threshold - 50) > 8 ? [50] : []), 100].map((p) => (
          <g key={p}>
            <line x1={L} x2={L + IW} y1={yAt(p)} y2={yAt(p)} className="grid-line" />
            <text x={L - 6} y={yAt(p) + 3} className="axis-txt" textAnchor="end">
              {p}
            </text>
          </g>
        ))}

        {/* Labelled on the axis, not floating over the plot. Set inside the
            chart it landed exactly where the data is densest and became
            unreadable text on top of unreadable lines. */}
        <line x1={L} x2={L + IW} y1={yAt(threshold)} y2={yAt(threshold)} className="thresh" />
        <text x={L - 6} y={yAt(threshold) + 3} className="axis-txt is-thresh" textAnchor="end">
          {threshold}
        </text>

        {ticks.map((label, i) =>
          label ? (
            <text
              key={i}
              x={L + (i / Math.max(ticks.length - 1, 1)) * IW}
              y={H - 7}
              className="axis-txt"
              textAnchor={i === 0 ? "start" : i === ticks.length - 1 ? "end" : "middle"}
            >
              {label}
            </text>
          ) : null,
        )}

        {drawable.map((s) => {
          const emph = isolated == null ? s.active : isolated === s.number;
          const dim = isolated != null && isolated !== s.number;
          return s.runs.map((run, i) => (
            <path
              key={`${s.number}-${i}`}
              d={run
                .map((p, k) => `${k === 0 ? "M" : "L"}${xAt(p.x).toFixed(1)} ${yAt(p.v).toFixed(1)}`)
                .join("")}
              className={`ln${emph ? " emph" : ""}${dim ? " dim" : ""}`}
              fill="none"
              onMouseEnter={() => setIsolated(s.number)}
            />
          ));
        })}

        {drawable.map((s) => {
          if (s.last == null) return null;
          const state = quotaState(s.last);
          const emph = isolated == null ? s.active : isolated === s.number;
          const lastRun = s.runs[s.runs.length - 1];
          const lastPt = lastRun?.[lastRun.length - 1];
          if (!lastPt) return null;
          const capX = xAt(lastPt.x);
          const capY = yAt(s.last);
          const labY = labelYFor(s.number);
          return (
            <g key={`cap-${s.number}`} onMouseEnter={() => setIsolated(s.number)}>
              <circle cx={capX} cy={capY} r={2.5} className={`endcap${state === "ok" ? "" : ` ${state}`}`} />
              {/* A leader only when the label had to move, so a label sitting
                  on its own line is not cluttered by a redundant stub. */}
              {Math.abs(labY - capY) > 2 && (
                <path d={`M${capX + 3} ${capY}L${L + IW + 4} ${labY}`} className="endleader" fill="none" />
              )}
              <text x={L + IW + 8} y={labY + 3.5} className={`endlab${emph ? " emph" : ""}`}>
                <title>{`${s.name} — ${Math.round(s.last)}%`}</title>
                {s.name.length > 15 ? `${s.name.slice(0, 14)}…` : s.name} {Math.round(s.last)}%
              </text>
            </g>
          );
        })}
      </svg>
    </div>
  );
}
