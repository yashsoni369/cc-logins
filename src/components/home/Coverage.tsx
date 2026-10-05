import type { CSSProperties } from "react";

import { useClockFormat } from "../../lib/clockFormat";
import type { CoveragePlan, Lane, Projection } from "../../lib/coverage";
import { pooledHeadroom } from "../../lib/dashboard";
import { formatRunway, type RunwayEstimate } from "../../lib/runway";
import { formatClock, formatDayClock } from "../../lib/time";
import { bindingUtilisation, displayName, headroom, type Account } from "../../types";

/** Stale or unknown figures dim to this — the value meters already use. */
const DIM: CSSProperties = { opacity: 0.55 };
/** Beyond this the runway is a floor, not a time — the same horizon as `formatRunway`. */
const HORIZON_SECONDS = 7 * 86_400;
const HOURS = 12;

const iso = (ms: number) => new Date(ms).toISOString();

interface CoverageKpisProps {
  accounts: Account[];
  runway: RunwayEstimate;
  activeProjection: Projection | null;
  bestNext: Account | null;
  autoSwitch: boolean;
  /** True when the most recent background refresh failed. */
  degraded: boolean;
  now: number;
}

/**
 * The three answers Home leads with: until when your accounts cover you,
 * what is in use and when it runs out, and which account is next.
 *
 * Every figure is a projection and is labelled as one. When nothing can be
 * read the answer is "unknown", never a confident zero; when nothing is
 * burning there is nothing to project, never "forever".
 */
export function CoverageKpis({ accounts, runway, activeProjection, bestNext, autoSwitch, degraded, now }: CoverageKpisProps) {
  const clockFormat = useClockFormat();
  const measurable = pooledHeadroom(accounts).usable > 0;
  const unknownRunway = runway.seconds == null;
  const stale = runway.degraded || degraded;
  const qualified = !measurable || unknownRunway || stale;
  const qualLabel = !measurable || unknownRunway ? "unknown" : stale ? "stale estimate" : "estimate";
  const active = accounts.find((a) => a.active) ?? null;
  const activeUtil = active ? bindingUtilisation(active.usage) : null;

  let cover: string;
  let coverSub: string;
  if (!measurable) {
    cover = "unknown";
    // A cold start reads exactly like a total failure — no usage anywhere —
    // and only `degraded` tells them apart: it is set when a refresh
    // actually failed, not while the first one is still in flight.
    coverSub = degraded ? "no usage could be read from any account" : "waiting for the first reading";
  } else if (unknownRunway) {
    cover = "idle";
    coverSub = "nothing is burning right now, so there is nothing to project";
  } else if ((runway.seconds ?? 0) > HORIZON_SECONDS) {
    cover = "> 7 days";
    coverSub = `at the current pace · from ${runway.contributing} of ${accounts.length} accounts`;
  } else {
    cover = formatDayClock(iso(now + (runway.seconds ?? 0) * 1_000), clockFormat, now) ?? "unknown";
    coverSub = `≈ ${formatRunway(runway.seconds)} at the current pace · from ${runway.contributing} of ${accounts.length} accounts`;
  }

  let usingSub = "no reading yet";
  let usingTone = "";
  if (active && activeUtil !== null) {
    const at = activeProjection?.at ?? null;
    if (at !== null && activeProjection?.beforeReset) {
      usingSub = at <= now ? "at its limit now" : `runs out ~${formatClock(iso(at), clockFormat, now) ?? "soon"}`;
      usingTone = activeUtil >= 90 || at - now < 3_600_000 ? "danger" : "caution";
    } else if (at !== null) {
      usingSub = `${Math.round(activeUtil)}% · resets before it fills`;
    } else {
      usingSub = `${Math.round(activeUtil)}% used`;
    }
  }

  return (
    <div className="kpis" aria-label="Coverage summary">
      <div className="kpi kpi-hero">
        <div className="kpi-l">
          Covered until <span className={`pill dash-qual${qualLabel === "estimate" ? "" : " caution"}`}>{qualLabel}</span>
        </div>
        <div className="kpi-v num" data-testid="coverage-value" style={qualified ? DIM : undefined}>
          {cover}
        </div>
        <div className="kpi-h">{coverSub}</div>
      </div>
      <div className="kpi">
        <div className="kpi-l">Using now</div>
        <div className="kpi-v kpi-name">{active ? displayName(active) : "none"}</div>
        <div className={`kpi-h ${usingTone}`}>{usingSub}</div>
      </div>
      <div className="kpi">
        <div className="kpi-l">{autoSwitch ? "Auto-switch would pick" : "Best next"}</div>
        <div className="kpi-v kpi-name">{bestNext ? displayName(bestNext) : "none ready"}</div>
        <div className="kpi-h">
          {bestNext
            ? `${Math.round(headroom(bestNext.usage) ?? 0)}% left`
            : "no other account has measured headroom"}
        </div>
      </div>
    </div>
  );
}

function laneNote(lane: Lane): string {
  if (lane.unknown) return "no reading";
  if (lane.unavailable) {
    const status = lane.account.usageStatus;
    return status === "disabled" ? "held out" : status === "reloginrequired" ? "needs sign-in" : "unavailable";
  }
  const util = bindingUtilisation(lane.account.usage);
  return util === null ? "" : `${Math.round(util)}% used`;
}

/**
 * The next twelve hours, one lane per account: when each is usable, when the
 * account in use is projected to hit its limit, when limits reset, and —
 * with auto-switch on — where it is expected to hand off.
 *
 * Only the account in use is projected; the others are drawn as they stand,
 * because nobody is spending them yet. The caption says so.
 */
export function CoverageTimeline({ plan, now, autoSwitch, degraded }: { plan: CoveragePlan; now: number; autoSwitch: boolean; degraded: boolean }) {
  const clockFormat = useClockFormat();
  const at = (hours: number) => formatClock(iso(now + hours * 3_600_000), clockFormat, now);
  const pct = (hours: number) => `${(hours / HOURS) * 100}%`;
  const ticks = [0, 3, 6, 9, 12];
  const target = plan.handoff ? plan.lanes.find((l) => l.account.number === plan.handoff?.to) : undefined;

  const caption = !autoSwitch
    ? "auto-switch is off · projection only"
    : plan.handoff && target
      ? `auto-switch expected to move to ${displayName(target.account)} around ${at(plan.handoff.at) ?? "then"}`
      : "auto-switch is on · no switch expected in this window";

  return (
    <section className="band cov" aria-label="Next 12 hours">
      <div className="band-head">
        <h2>Next 12 hours</h2>
        <span className="sub">{caption}</span>
      </div>
      <div className="cov-grid" style={degraded ? DIM : undefined}>
        <div className="cov-axis" aria-hidden="true">
          <span />
          <div className="cov-ticks">
            {ticks.map((t) => (
              <span key={t} className="cov-tick num" style={{ left: pct(t) }}>
                {t === 0 ? "now" : (at(t) ?? `+${t}h`)}
              </span>
            ))}
          </div>
        </div>
        {plan.lanes.map((lane) => (
          <div key={lane.account.number} className={`cov-lane${lane.account.active ? " is-active" : ""}${lane.unavailable ? " is-off" : ""}`}>
            <div className="cov-who">
              <span className="cov-name">{displayName(lane.account)}</span>
              <span className="cov-note num">{laneNote(lane)}</span>
            </div>
            <div className="cov-track" role="img" aria-label={`${displayName(lane.account)}: ${laneNote(lane)}`}>
              {ticks.slice(1, -1).map((t) => (
                <span key={t} className="cov-gridline" style={{ left: pct(t) }} />
              ))}
              {lane.unknown && <span className="cov-seg unknown" style={{ left: 0, width: "100%" }} />}
              {lane.segments.map((seg, i) => (
                <span key={i} className={`cov-seg ${seg.state}`} style={{ left: pct(seg.from), width: pct(seg.to - seg.from) }} />
              ))}
              {lane.limit !== null && (
                <span className="cov-mark limit" style={{ left: pct(lane.limit) }}>
                  <span>limit {at(lane.limit)}</span>
                </span>
              )}
              {lane.reset !== null && (
                <span className="cov-mark reset" style={{ left: pct(lane.reset) }} title={`Resets ${at(lane.reset) ?? ""}`}>
                  <span>reset</span>
                </span>
              )}
              {plan.handoff && plan.handoff.to === lane.account.number && (
                <span className="cov-mark handoff" style={{ left: pct(plan.handoff.at) }}>
                  <span>switch ~{at(plan.handoff.at)}</span>
                </span>
              )}
            </div>
          </div>
        ))}
      </div>
      <div className="cov-legend">
        <span><i className="free" />usable</span>
        <span><i className="inUse" />current</span>
        <span><i className="limited" />at limit</span>
        <span><i className="reset" />reset</span>
        <span className="cov-foot">Only the current account is projected, from its last few hours. Others are shown as they stand now.</span>
      </div>
    </section>
  );
}
