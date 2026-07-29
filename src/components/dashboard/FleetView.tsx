import type { CSSProperties, KeyboardEvent } from "react";
import type { Account, Sample } from "../../types";
import { ageLabel, displayName, quotaState } from "../../types";
import type { RunwayEstimate } from "../../lib/runway";
import { formatRunway } from "../../lib/runway";
import { formatCountdown } from "../../lib/time";
import UsageMeter from "../UsageMeter";
import ResetStagger from "./ResetStagger";

interface FleetViewProps {
  accounts: Account[];
  /** Last 24h of samples per account key. A missing or empty entry is normal. */
  samplesByKey: Map<string, Sample[]>;
  /** Stable key, `undefined` while it is still being resolved. */
  keyFor: (a: Account) => string | undefined;
  runway: RunwayEstimate;
  onOpenAccount: (accountKey: string, account: Account) => void;
  /** True when the most recent background refresh failed. */
  degraded: boolean;
  now: number;
}

const SPARK_W = 96;
const SPARK_H = 24;
const SPARK_PAD = 2;
const WINDOW_MS = 24 * 3_600_000;
/**
 * A gap this long breaks the line. Same rule as BurnRateChart: drawing straight
 * through unobserved time invents a trend nobody measured.
 */
const GAP_MS = 45 * 60_000;

/** Stale meters and projections dim to this — the accounts table's own value. */
const DIM: CSSProperties = { opacity: 0.55 };

interface Point {
  x: number;
  y: number;
}

/** Contiguous runs of samples, plus the most recent reading. */
function spark(samples: Sample[], now: number): { runs: Point[][]; lastPct: number | null } {
  const start = now - WINDOW_MS;
  // Sorted defensively: an out-of-order sample would draw a path that zig-zags
  // backwards through time, which reads as violent churn that never happened.
  const points = samples
    .map((s) => ({ ms: Date.parse(s.timestamp), pct: s.bindingPct }))
    .filter((p) => Number.isFinite(p.ms) && p.ms >= start && p.ms <= now)
    .sort((a, b) => a.ms - b.ms);

  const runs: Point[][] = [];
  let current: Point[] = [];
  let prev = 0;
  let lastPct: number | null = null;

  for (const p of points) {
    if (current.length && p.ms - prev > GAP_MS) {
      runs.push(current);
      current = [];
    }
    const pct = Math.max(0, Math.min(100, p.pct));
    current.push({
      x: SPARK_PAD + ((p.ms - start) / WINDOW_MS) * (SPARK_W - SPARK_PAD * 2),
      y: SPARK_H - SPARK_PAD - (pct / 100) * (SPARK_H - SPARK_PAD * 2),
    });
    prev = p.ms;
    lastPct = pct;
  }
  if (current.length) runs.push(current);

  return { runs, lastPct };
}

/** A lone sample becomes a zero-length segment so the round cap renders a dot. */
function pathFor(run: Point[]): string {
  const first = run[0];
  if (!first) return "";
  if (run.length === 1) return `M${first.x.toFixed(1)} ${first.y.toFixed(1)}L${first.x.toFixed(1)} ${first.y.toFixed(1)}`;
  return run.map((p, i) => `${i === 0 ? "M" : "L"}${p.x.toFixed(1)} ${p.y.toFixed(1)}`).join("");
}

function Sparkline({ samples, now, label }: { samples: Sample[]; now: number; label: string }) {
  const { runs, lastPct } = spark(samples, now);

  // No samples leaves the area empty. A baseline drawn at zero would claim a
  // confirmed-idle day, which is the opposite of "we have no readings".
  if (!runs.length || lastPct == null) {
    return (
      <div className="fleet-spark" title={`No usage recorded for ${label} in the last 24 hours`}>
        <span className="pct-unknown">··</span>
      </div>
    );
  }

  const state = quotaState(lastPct);
  const stateClass = state === "ok" ? "" : ` ${state}`;
  const end = runs[runs.length - 1]?.slice(-1)[0];
  const title = `${label}: binding utilisation over the last 24 hours, now ${Math.round(lastPct)}%`;

  return (
    <div className="fleet-spark">
      <svg viewBox={`0 0 ${SPARK_W} ${SPARK_H}`} preserveAspectRatio="xMidYMid meet" role="img" aria-label={title}>
        <title>{title}</title>
        {runs.map((run, i) => (
          <path key={i} d={pathFor(run)} className={`chart-line${stateClass}`} fill="none" />
        ))}
        {end && <circle cx={end.x} cy={end.y} r={2} className={`chart-dot${stateClass}`} />}
      </svg>
    </div>
  );
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
 * Where you stand, in one screen: how long the fleet lasts, who is carrying it,
 * and when relief arrives.
 *
 * The headline is a projection, never a measurement, and it says so — an
 * unqualified runway figure built on stale or partial samples is the worst lie
 * this app can tell, because it reads as reassurance.
 */
export default function FleetView({
  accounts,
  samplesByKey,
  keyFor,
  runway,
  onOpenAccount,
  degraded,
  now,
}: FleetViewProps) {
  const unknownRunway = runway.seconds == null;
  const stale = runway.degraded || degraded;
  const qualified = unknownRunway || stale;
  const qualLabel = unknownRunway ? "unknown" : stale ? "stale estimate" : "estimate";

  const burn =
    runway.pctPerHour == null
      ? "burn unknown"
      : `${runway.pctPerHour < 10 ? runway.pctPerHour.toFixed(1) : Math.round(runway.pctPerHour)}%/h`;
  const nextReset = formatCountdown(soonestReset(accounts), now) ?? "unknown";
  const meterStyle = degraded ? DIM : undefined;

  return (
    <>
      <div className="dash-headline">
        {/* Caption first so the figure is never announced as a bare number. */}
        <span className="dash-cap">pooled runway</span>
        {/* Dimming is the same treatment a stale meter gets in the accounts
            table: a qualified figure must not look freshly confirmed. */}
        <span className="dash-big num" style={qualified ? DIM : undefined}>
          {formatRunway(runway.seconds)}
        </span>
        <span className={`pill dash-qual${qualified ? " caution" : ""}`}>{qualLabel}</span>
        <span className="dash-cap">
          <span className="num">{burn}</span>
          <span className="num">next reset {nextReset}</span>
          {/* Auditable: which accounts the projection was actually able to use. */}
          <span className="num">
            from {runway.contributing} of {accounts.length} accounts
          </span>
        </span>
      </div>

      {accounts.length > 0 && (
        <div className="fleet-head">
          <span>Account</span>
          <span>Last 24 h</span>
          <span>7-day</span>
          <span className="r">Resets in</span>
        </div>
      )}

      {accounts.map((account) => {
        const accountKey = keyFor(account);
        const name = displayName(account);
        const isHeldOut = account.usageStatus === "disabled";
        const needsRelogin = account.usageStatus === "reloginrequired";
        const age = ageLabel(account.usageAgeSeconds);
        const fiveHour = account.usage?.fiveHour;
        // Same fallback chain as the accounts table: the backend's own drifting
        // string beats a blank cell, and a blank cell beats an invented one.
        const resets = formatCountdown(fiveHour?.resetsAt, now) ?? fiveHour?.countdown ?? "—";
        const sevenDay = account.usage?.sevenDay?.pct;

        // Unresolved key means we cannot address the account yet, so the row is
        // inert rather than a control that does nothing when pressed.
        const open = accountKey ? () => onOpenAccount(accountKey, account) : undefined;
        const label = [
          name,
          account.active ? "active" : null,
          isHeldOut ? "held out of rotation" : null,
          needsRelogin ? "re-login required" : null,
          sevenDay == null ? "7-day usage unknown" : `7-day ${Math.round(sevenDay)}%`,
          resets === "—" ? "reset time unknown" : `resets in ${resets}`,
          age,
          "open account",
        ]
          .filter(Boolean)
          .join(", ");

        return (
          <div
            key={account.number}
            className={`fleet-row${account.active ? " is-active" : ""}`}
            {...(open
              ? {
                  role: "button" as const,
                  tabIndex: 0,
                  "aria-label": label,
                  onClick: open,
                  onKeyDown: (e: KeyboardEvent<HTMLDivElement>) => {
                    if (e.key === "Enter" || e.key === " ") {
                      e.preventDefault();
                      open();
                    }
                  },
                }
              : {})}
          >
            <div className="fleet-who">
              <span className={`mark${account.active ? " on" : ""}`} />
              <span className="alias">{name}</span>
              {account.active && <span className="pill on">active</span>}
              {isHeldOut && <span className="pill">held out</span>}
              {needsRelogin && <span className="pill danger">re-login required</span>}
              {age && <span className="pill">{age}</span>}
            </div>

            <Sparkline samples={(accountKey && samplesByKey.get(accountKey)) || []} now={now} label={name} />

            <div style={meterStyle}>
              <UsageMeter pct={sevenDay} />
            </div>

            <div className={`fleet-reset num${resets === "—" ? " pct-unknown" : ""}`}>{resets}</div>
          </div>
        );
      })}

      <ResetStagger accounts={accounts} now={now} />
    </>
  );
}
