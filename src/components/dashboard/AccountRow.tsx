import { useId, useState } from "react";

import { useClockFormat } from "@/lib/clockFormat";
import { formatClock, formatCountdown } from "@/lib/time";
import {
  bindingUtilisation,
  bindingWindow,
  displayName,
  formatSpend,
  isEnterprise,
  quotaState,
  type Account,
  type DayStat,
  type Sample,
  type ScopedSample,
} from "@/types";
import { Loading } from "@/components/Loading";
import UsageMeter from "@/components/UsageMeter";
import PlanBadge from "@/components/PlanBadge";
import UsageHeatmap from "./UsageHeatmap";
import { RangeChart, Sparkline, WindowChart } from "./charts";
import type { FleetSeries } from "@/lib/dashboard";

/** One absence, one sentence — the wording HistoryScreen already uses. */
const NO_HISTORY = "No history yet for this account — usage is recorded from now on.";

type Tab = "windows" | "models" | "rhythm";

/** A per-model weekly window as this row renders it. `resetsAt` is absent when
 *  the figures came from a recorded sample rather than the live snapshot. */
interface ModelRow {
  name: string;
  pct: number;
  resetsAt?: string;
}

export interface AccountDetail {
  samples: Sample[];
  daily: DayStat[];
  loading: boolean;
}

interface AccountRowProps {
  account: Account;
  detail: AccountDetail | undefined;
  /** This account's line from the rotation chart, reused for the row sparkline
   *  so opening the dashboard does not fetch the same history twice. */
  series: FleetSeries | undefined;
  expanded: boolean;
  onToggle: () => void;
  /** Auto-switch threshold, drawn as a hairline on every chart in the drawer. */
  threshold: number;
  rangeDays: number;
  /** Peers offered by the compare control. */
  peers: Account[];
  degraded: boolean;
  now: number;
}

/** The account's headline state in words — hue alone never carries it. */
function statusLabel(account: Account): string | null {
  switch (account.usageStatus) {
    case "disabled":
      return "held out";
    case "reloginrequired":
    case "expired":
      return "re-login";
    case "foreigncredential":
      return "mismatch";
    case "stale":
      return "last known";
    case "unavailable":
      return "unavailable";
    case "error":
      return "error";
    case "unknown":
      return "never measured";
    default:
      return null;
  }
}

function Stat({ k, v, tone }: { k: string; v: string; tone?: string }) {
  return (
    <div className="stat">
      <span className="k">{k}</span>
      <span className={`v${tone ? ` ${tone}` : ""}`}>{v}</span>
    </div>
  );
}

/**
 * One fleet row that opens in place.
 *
 * Opening used to replace the whole screen, which cost the reader the fleet
 * they were comparing against and reset their scroll. A disclosure keeps the
 * neighbouring rows on screen, which is the only reason to look at one
 * account here rather than on the Accounts screen.
 */
export default function AccountRow({
  account,
  detail,
  series,
  expanded,
  onToggle,
  threshold,
  rangeDays,
  peers,
  degraded,
  now,
}: AccountRowProps) {
  const [tab, setTab] = useState<Tab>("windows");
  const [compare, setCompare] = useState<string>("");
  const clockFormat = useClockFormat();
  const drawerId = useId();

  const name = displayName(account);
  const binding = bindingUtilisation(account.usage);
  const state = quotaState(binding);
  const cls = state === "ok" ? "" : state;
  const status = statusLabel(account);
  const sevenDay = account.usage?.sevenDay?.pct;
  // Read from the binding window rather than fiveHour: on an enterprise
  // account there is no five-hour window, and its monthly cap is the only
  // reset there is.
  const bindingWin = bindingWindow(account.usage);
  const resets = formatCountdown(bindingWin?.resetsAt, now) ?? bindingWin?.countdown ?? "—";
  const enterprise = isEnterprise(account.usage);
  const spend = account.usage?.spend;

  const samples = detail?.samples ?? [];
  const daily = detail?.daily ?? [];
  const comparedTo = peers.find((p) => String(p.number) === compare);

  const label = [
    name,
    account.active ? "active" : null,
    status,
    binding == null ? "usage unknown" : `${Math.round(binding)}% used`,
    resets === "—" ? "reset time unknown" : `resets in ${resets}`,
    expanded ? "collapse" : "expand",
  ]
    .filter(Boolean)
    .join(", ");

  /**
   * Live usage first, recorded sample second.
   *
   * Only the live window carries `resetsAt` — `scoped_samples` stores name and
   * percentage alone, so a recorded row can show the level but genuinely does
   * not know when it clears. Falling back rather than merging keeps every
   * figure in this list read at one instant.
   */
  const models: ModelRow[] =
    account.usage?.scoped?.map((m) => ({ name: m.name, pct: m.pct, resetsAt: m.resetsAt })) ??
    (samples[samples.length - 1]?.scoped ?? []).map((m: ScopedSample) => ({ name: m.name, pct: m.pct }));
  const firstAt = samples[0]?.timestamp;
  const lastAt = samples[samples.length - 1]?.timestamp;

  return (
    <div className="row-shell">
      <button
        type="button"
        className="dash-row"
        aria-expanded={expanded}
        aria-controls={drawerId}
        aria-label={label}
        onClick={onToggle}
      >
        <span className="chev" aria-hidden="true">
          ▶
        </span>
        <span className="row-who">
          <span className={`mark${account.active ? " on" : ""}`} />
          <span className="row-name" title={name}>
            {name}
          </span>
          <PlanBadge usage={account.usage} />
          {account.active && <span className="pill on">active</span>}
          {status && <span className={`pill${status === "re-login" || status === "mismatch" ? " danger" : ""}`}>{status}</span>}
        </span>
        <Sparkline runs={series?.runs ?? []} last={series?.last ?? null} label={name} />
        <span className="row-meter" style={degraded ? { opacity: 0.55 } : undefined}>
          <UsageMeter pct={binding} />
          <small>binding</small>
        </span>
        {enterprise && spend ? (
          <span className="row-cell" title={formatSpend(spend)}>
            {formatSpend(spend).split(" of ")[0]}
            <small>of {formatSpend(spend).split(" of ")[1]}</small>
          </span>
        ) : (
          <span className="row-cell">
            {sevenDay == null ? "··" : `${Math.round(sevenDay)}%`}
            <small>7-day</small>
          </span>
        )}
        <span className={`row-cell${resets === "—" ? " pct-unknown" : ""}`}>
          {resets}
          <small>resets in</small>
        </span>
      </button>

      {/* Always in the tree so `aria-controls` resolves; the class alone
          governs visibility. `hidden` would fight the class's own display. */}
      <div className={`drawer${expanded ? " open" : ""}`} id={drawerId}>
        {expanded && (
          <>
            <div className="tabs" role="tablist" aria-label={`${name} detail`}>
              <button type="button" role="tab" aria-selected={tab === "windows"} onClick={() => setTab("windows")}>
                Windows
              </button>
              <button type="button" role="tab" aria-selected={tab === "models"} onClick={() => setTab("models")}>
                Models
              </button>
              <button type="button" role="tab" aria-selected={tab === "rhythm"} onClick={() => setTab("rhythm")}>
                Rhythm
              </button>
              <span className="spacer" />
              {peers.length > 0 && (
                <select
                  className="narrow"
                  aria-label={`Compare ${name} against`}
                  value={compare}
                  onChange={(e) => setCompare(e.target.value)}
                >
                  <option value="">compare: none</option>
                  {peers.map((p) => (
                    <option key={p.number} value={String(p.number)}>
                      compare: {displayName(p)}
                    </option>
                  ))}
                </select>
              )}
            </div>

            {detail?.loading ? (
              <Loading label={`Loading ${name}'s history`} />
            ) : samples.length === 0 && daily.length === 0 ? (
              <div className="empty">
                <p>{NO_HISTORY}</p>
              </div>
            ) : (
              <>
                {tab === "windows" && (
                  <>
                    {/* An enterprise plan has no rate-limit windows at all, so the
                        two window charts would draw from sample columns that are
                        null for every reading. One panel for the limit that does
                        exist says more than two empty ones. */}
                    {enterprise && spend ? (
                      <>
                        <WindowChart
                          label="Spend cap"
                          samples={samples}
                          pick={(sample) => sample.bindingPct}
                          threshold={threshold}
                          startLabel={formatClock(firstAt, clockFormat) ?? "earlier"}
                          endLabel={formatClock(lastAt, clockFormat) ?? "now"}
                        />
                        <div className="stats">
                          <Stat k="Spent" v={formatSpend(spend).split(" of ")[0] ?? "··"} />
                          <Stat k="Monthly cap" v={formatSpend(spend).split(" of ")[1] ?? "··"} />
                          <Stat k="Used" v={`${Math.round(spend.pct)}%`} tone={cls} />
                          <Stat k="Resets" v={resets === "—" ? "unknown" : resets} />
                        </div>
                        <p className="dash-note">
                          This plan bills usage against a monthly spend cap and has no
                          5-hour or 7-day windows. The reset is the start of the next
                          month — the usage API does not supply one.
                        </p>
                      </>
                    ) : samples.length > 0 ? (
                      <div className="duo">
                        <WindowChart
                          label="5-hour window"
                          samples={samples}
                          pick={(s) => s.fiveHourPct}
                          threshold={threshold}
                          startLabel={formatClock(firstAt, clockFormat) ?? "earlier"}
                          endLabel={formatClock(lastAt, clockFormat) ?? "now"}
                        />
                        <WindowChart
                          label="7-day window"
                          samples={samples}
                          pick={(s) => s.sevenDayPct}
                          threshold={threshold}
                          startLabel={formatClock(firstAt, clockFormat) ?? "earlier"}
                          endLabel={formatClock(lastAt, clockFormat) ?? "now"}
                        />
                      </div>
                    ) : (
                      <div className="empty">
                        <p>{NO_HISTORY}</p>
                      </div>
                    )}
                    {daily.length > 0 && (
                      <div style={{ marginTop: 18 }}>
                        <RangeChart daily={daily} rangeDays={rangeDays} threshold={threshold} />
                      </div>
                    )}
                    {comparedTo && (
                      <p className="dash-note">
                        {displayName(comparedTo)} is at{" "}
                        <span className="num">
                          {bindingUtilisation(comparedTo.usage) == null
                            ? "an unknown level"
                            : `${Math.round(bindingUtilisation(comparedTo.usage) ?? 0)}%`}
                        </span>{" "}
                        right now, against {name}&rsquo;s{" "}
                        <span className="num">{binding == null ? "unknown" : `${Math.round(binding)}%`}</span>.
                      </p>
                    )}
                  </>
                )}

                {tab === "models" && (
                  <>
                    {models.length === 0 ? (
                      <div className="empty">
                        <p>No per-model windows reported for this account.</p>
                      </div>
                    ) : (
                      <div className="mrows">
                        {models.map((m) => {
                          const mState = quotaState(m.pct);
                          return (
                            <div className="mrow" key={m.name}>
                              <span className="mn" title={m.name}>
                                {m.name}
                              </span>
                              <span className="mt">
                                <i
                                  className={mState === "ok" ? "" : mState}
                                  style={{ width: `${Math.max(0, Math.min(100, m.pct))}%` }}
                                />
                              </span>
                              <span className="mv">{Math.round(m.pct)}%</span>
                              <span className="mr">{formatCountdown(m.resetsAt, now) ?? "—"}</span>
                            </div>
                          );
                        })}
                      </div>
                    )}
                    <p className="dash-note">
                      Per-model weekly limits, as the usage API reports them. Each has its own
                      reset, so the earliest one here is what actually gates this account.
                    </p>
                  </>
                )}

                {tab === "rhythm" && (
                  <>
                    <UsageHeatmap samples={samples} />
                    <div className="stats">
                      <Stat k="Peak in view" v={statOf(samples, "peak")} tone={cls} />
                      <Stat k="Average" v={statOf(samples, "avg")} />
                      <Stat k="Readings" v={String(samples.length)} />
                      <Stat k="Days measured" v={`${daily.length} of ${rangeDays}`} />
                    </div>
                  </>
                )}
              </>
            )}
          </>
        )}
      </div>
    </div>
  );
}

/** Peak/average of the binding series, or "··" when nothing was measured. */
function statOf(samples: Sample[], kind: "peak" | "avg"): string {
  const values = samples.map((s) => s.bindingPct).filter((v) => Number.isFinite(v));
  if (values.length === 0) return "··";
  const value =
    kind === "peak" ? Math.max(...values) : values.reduce((t, v) => t + v, 0) / values.length;
  return `${Math.round(value)}%`;
}
