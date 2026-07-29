import { useState } from "react";

import type { FleetSeries, RangeSpec } from "@/lib/dashboard";
import { quotaState } from "@/types";

const W = 760;
const H = 190;
const L = 30;
/** Room for the direct end labels, which replace a colour legend. */
const R = 92;
const T = 10;
const B = 24;

const IW = W - L - R;
const IH = H - T - B;

const xAt = (x: number) => L + x * IW;
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
  const drawable = series.filter((s) => s.runs.length > 0);

  if (drawable.length === 0) {
    return (
      <div className="empty">
        <p>No usage recorded in the {spec.phrase} — history is written as the app runs.</p>
      </div>
    );
  }

  const ticks = TICKS[spec.key] ?? TICKS["7d"] ?? [];
  // Labels are stacked in series order but nudged apart when two lines end at
  // the same height, which is common once several accounts sit near zero.
  const placed: number[] = [];
  const labelY = (v: number) => {
    let y = yAt(v);
    while (placed.some((p) => Math.abs(p - y) < 11)) y += 11;
    placed.push(y);
    return y;
  };

  return (
    <div className="chartwrap">
      <svg
        className="fleetchart"
        viewBox={`0 0 ${W} ${H}`}
        preserveAspectRatio="none"
        role="img"
        aria-label={`Binding utilisation per account over the ${spec.phrase}`}
        onMouseLeave={() => setIsolated(null)}
      >
        {[0, 50, 100].map((p) => (
          <g key={p}>
            <line x1={L} x2={L + IW} y1={yAt(p)} y2={yAt(p)} className="grid-line" />
            <text x={L - 6} y={yAt(p) + 3} className="axis-txt" textAnchor="end">
              {p}
            </text>
          </g>
        ))}

        <line x1={L} x2={L + IW} y1={yAt(threshold)} y2={yAt(threshold)} className="thresh" />
        <text x={L + 3} y={yAt(threshold) - 4} className="axis-txt">
          switch threshold {threshold}
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
          return (
            <g key={`cap-${s.number}`} onMouseEnter={() => setIsolated(s.number)}>
              <circle
                cx={xAt(lastPt.x)}
                cy={yAt(s.last)}
                r={2.5}
                className={`endcap${state === "ok" ? "" : ` ${state}`}`}
              />
              <text x={xAt(lastPt.x) + 7} y={labelY(s.last) + 3.5} className={`endlab${emph ? " emph" : ""}`}>
                {s.name.length > 11 ? `${s.name.slice(0, 10)}…` : s.name} {Math.round(s.last)}%
              </text>
            </g>
          );
        })}
      </svg>
    </div>
  );
}
