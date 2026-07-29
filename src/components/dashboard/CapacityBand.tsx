import type { CSSProperties } from "react";

import { formatRunway, type RunwayEstimate } from "@/lib/runway";
import { formatCountdown } from "@/lib/time";
import { pooledHeadroom } from "@/lib/dashboard";
import { quotaState, type Account } from "@/types";

/** Stale meters and projections dim to this — the accounts table's own value. */
const DIM: CSSProperties = { opacity: 0.55 };

interface CapacityBandProps {
  accounts: Account[];
  runway: RunwayEstimate;
  /** True when the most recent background refresh failed. */
  degraded: boolean;
  now: number;
}

/** Earliest 5-hour reset across the fleet, as its own ISO instant. */
function soonestReset(accounts: Account[]): string | undefined {
  let best: number | null = null;
  let iso: string | undefined;
  for (const a of accounts) {
    const resetsAt = a.usage?.fiveHour?.resetsAt;
    const ms = resetsAt ? Date.parse(resetsAt) : Number.NaN;
    if (!Number.isFinite(ms)) continue;
    if (best === null || ms < best) {
      best = ms;
      iso = resetsAt;
    }
  }
  return iso;
}

/**
 * How long the fleet lasts, and where the remaining capacity actually sits.
 *
 * The runway figure is a projection, never a measurement, and says so — an
 * unqualified number built on stale or partial samples is the worst lie this
 * app can tell, because it reads as reassurance. The segmented bar beside it
 * answers the follow-up question the figure always provokes: *whose* headroom
 * is that, and can the switcher even reach it.
 */
export default function CapacityBand({ accounts, runway, degraded, now }: CapacityBandProps) {
  const headroom = pooledHeadroom(accounts);
  const unknownRunway = runway.seconds == null;
  const stale = runway.degraded || degraded;
  const qualified = unknownRunway || stale;
  const qualLabel = unknownRunway ? "unknown" : stale ? "stale estimate" : "estimate";

  const burn =
    runway.pctPerHour == null
      ? "burn unknown"
      : `${runway.pctPerHour < 10 ? runway.pctPerHour.toFixed(1) : Math.round(runway.pctPerHour)}%/h burn`;
  const nextReset = formatCountdown(soonestReset(accounts), now) ?? "unknown";

  return (
    <section className="band">
      <div className="band-head">
        <h2>Capacity</h2>
        <span className="sub">how long the fleet lasts</span>
      </div>

      <div className="cap">
        <div className="cap-figure">
          {/* Caption first so the figure is never announced as a bare number. */}
          <span className="lab">pooled runway</span>
          <span className="cap-big num" style={qualified ? DIM : undefined}>
            {formatRunway(runway.seconds)}
          </span>
        </div>

        <div className="cap-meta">
          <span className={`pill dash-qual${qualified ? " caution" : ""}`}>{qualLabel}</span>
          <span className="num">{burn}</span>
          <span className="num">next reset {nextReset}</span>
          {/* Auditable: which accounts the projection was able to use at all. */}
          <span className="num">
            from {runway.contributing} of {accounts.length} accounts
          </span>
        </div>

        {headroom.total > 0 && (
          <div className="pool">
            <div className="pool-bar" style={degraded ? DIM : undefined}>
              {headroom.segments.map((seg) => {
                const state = quotaState(seg.binding);
                const cls = seg.binding == null ? "unknown" : state === "ok" ? "" : state;
                const title =
                  seg.binding == null
                    ? `${seg.name}: usage could not be read`
                    : `${seg.name}: ${Math.round(seg.free)}% of its own quota left` +
                      (seg.excluded ? " — held out of rotation" : "");
                return (
                  <div
                    key={seg.number}
                    className={`pool-seg ${cls}${seg.excluded ? " out" : ""}`}
                    // Width tracks free capacity, so the bar shrinks as the
                    // fleet is spent rather than merely recolouring.
                    style={{ flexGrow: Math.max(seg.free, 4) }}
                    title={title}
                  >
                    <i style={{ width: `${Math.max(0, Math.min(100, seg.free))}%` }} />
                  </div>
                );
              })}
            </div>
            <div className="pool-key">
              {headroom.segments.map((seg) => (
                <span key={seg.number}>
                  <b>{seg.name}</b>{" "}
                  <span className="num">{seg.binding == null ? "··" : `${Math.round(seg.free)}%`}</span>
                  {seg.excluded && seg.binding != null ? " held out" : ""}
                </span>
              ))}
            </div>
          </div>
        )}
      </div>
    </section>
  );
}
