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
  /** At least one account's usage was readable, so the pooled figure means
   *  something. Without this, "0% headroom" and "we could not read any of it"
   *  render identically. */
  const measurable = headroom.usable > 0;
  const unknownRunway = runway.seconds == null;
  const usableSegments = headroom.segments.filter((seg) => !seg.excluded);
  const excluded = headroom.segments.filter((seg) => seg.excluded);
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
        {/*
          Three states, and the third is the one that matters.

          `accountBurn` reports null when the slope is flat or falling — "not
          burning right now", never an infinite runway — so on a quiet machine
          the projection is legitimately unknown, and the measured headroom is
          the better headline. But when no account's usage can be read at all,
          the pooled figure is 0 by construction, and rendering that would put
          "0%" where the truth is "we do not know". Nothing here is allowed to
          turn an absent reading into a confident zero.
        */}
        <div className="cap-figure">
          <span className="lab">{measurable && unknownRunway ? "pooled headroom" : "pooled runway"}</span>
          <span className="cap-big num" style={qualified ? DIM : undefined}>
            {!measurable
              ? "unknown"
              : unknownRunway
                ? `${Math.round(headroom.pooled)}%`
                : formatRunway(runway.seconds)}
          </span>
          <span className="cap-sub num">
            {!measurable
              ? /*
                  A cold start reads exactly like a total failure: no usage on
                  any account. The poller's first fetch can take well over a
                  minute, and for that whole time the screen was a wall of
                  "unknown" with nothing to say why. `degraded` is the one
                  signal that separates them — it is set when a refresh
                  actually failed, not when none has finished yet.
                */
                degraded
                ? "no usage could be read from any account"
                : "waiting for the first reading"
              : unknownRunway
                ? "runway unknown — nothing is burning right now"
                : `${Math.round(headroom.pooled)}% headroom pooled`}
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

      </div>

      {/*
        Its own full-width row.

        Squeezed in beside the runway figure it had whatever space was left
        over — a few hundred pixels for the whole fleet — and it was the first
        thing on the screen nobody could read. It also scales: the track is the
        fleet's total capacity, so twenty accounts divide one bar rather than
        overflowing a strip.
      */}
      {headroom.usable > 0 && (
        <div className="pool">
          <div className="pool-head">
            <span className="lab">headroom by account</span>
            <span className="pool-total num">
              {Math.round(headroom.pooled)}% of {headroom.capacity}% ·{" "}
              {headroom.usable} account{headroom.usable === 1 ? "" : "s"} in rotation
            </span>
          </div>

          {/*
            The track is the whole fleet's capacity and the fill is what is
            left, so length finally carries meaning. Sized to the container
            before, it drew 12 points of headroom exactly as long as 290.
          */}
          <div className="pool-bar" style={degraded ? DIM : undefined}>
            {usableSegments
              .map((seg) => {
                const state = quotaState(seg.binding);
                const cls = state === "ok" ? "" : state;
                return (
                  <div
                    key={seg.number}
                    className={`pool-seg ${cls}`}
                    style={{ width: `${(seg.free / headroom.capacity) * 100}%` }}
                    title={`${seg.name}: ${Math.round(seg.free)}% of its own quota left`}
                  />
                );
              })}
            <div
              className="pool-spent"
              style={{ width: `${(headroom.spent / headroom.capacity) * 100}%` }}
              title={`${Math.round(headroom.spent)}% of the fleet's capacity already used`}
            />
          </div>

          {/*
            Labels in a second row on the same widths, directly beneath the
            segment each names.

            Every segment is the same colour by design — `tokens.css` reserves
            hue for quota state, so tinting per account would make a healthy
            one read as a different state. That left four identical blocks with
            no way to tell whose was whose. Position answers it instead, and it
            keeps working at twenty accounts where a legend would not: a narrow
            cell simply truncates rather than dropping out.
          */}
          <div className="pool-labels" aria-hidden="true">
            {usableSegments.map((seg) => (
              <span
                key={seg.number}
                className="pool-lbl"
                style={{ width: `${(seg.free / headroom.capacity) * 100}%` }}
              >
                <b>{seg.name}</b>
                <span className="num">{Math.round(seg.free)}%</span>
              </span>
            ))}
            <span className="pool-lbl is-spent" style={{ width: `${(headroom.spent / headroom.capacity) * 100}%` }}>
              <b>spent</b>
              <span className="num">{Math.round(headroom.spent)}%</span>
            </span>
          </div>

          {/* Listed but never drawn: they are not capacity the switcher can
              reach, so they are named rather than given a share of the bar. */}
          {excluded.length > 0 && (
            <p className="pool-out">
              Not in rotation:{" "}
              {excluded.map((seg, i) => (
                <span key={seg.number}>
                  {i > 0 && ", "}
                  <b>{seg.name}</b>
                  {seg.binding == null ? " (usage unreadable)" : " (held out)"}
                </span>
              ))}
            </p>
          )}
        </div>
      )}
    </section>
  );
}
