import { useEffect, useState } from "react";
import BurnRateChart, { type BurnRateSeries } from "./BurnRateChart";
import { mockHistoryRanges, type HistoryRangeId } from "../lib/mock";
import { getAccounts, historyAvailable, historySeries, historySummary } from "../lib/api";
import { displayName, stableKey, type DayStat, type HistorySummary } from "../types";

const RANGE_ORDER: HistoryRangeId[] = ["7d", "30d", "90d"];

const RANGE_DAYS: Record<HistoryRangeId, number> = { "7d": 7, "30d": 30, "90d": 90 };
const RANGE_META: Record<HistoryRangeId, { label: string; startLabel: string }> = {
  "7d": { label: "last 7 days", startLabel: "7 days ago" },
  "30d": { label: "last 30 days", startLabel: "30 days ago" },
  "90d": { label: "last 90 days", startLabel: "90 days ago" },
};

/**
 * One entry per day in the trailing `days` days, oldest first, filled from
 * `stats` where a matching day exists and `null` everywhere else. A missing
 * day is a genuine absence of measurement (the app wasn't running, most
 * likely) — it must never be coerced to 0, which would read as "confirmed
 * idle" rather than "unknown". See `BurnRateChart`'s `data` doc comment.
 */
function fillDailySeries(stats: DayStat[], days: number): Array<number | null> {
  const byDay = new Map(stats.map((s) => [s.day, s.avgPct]));
  const today = new Date();
  const out: Array<number | null> = [];
  for (let offset = days - 1; offset >= 0; offset--) {
    const d = new Date(Date.UTC(today.getUTCFullYear(), today.getUTCMonth(), today.getUTCDate()));
    d.setUTCDate(d.getUTCDate() - offset);
    const key = d.toISOString().slice(0, 10);
    out.push(byDay.get(key) ?? null);
  }
  return out;
}

interface LoadedState {
  live: boolean;
  /** False when the backend is present but the local history database could not be opened. */
  available: boolean;
  summary: HistorySummary | null;
  series: Array<BurnRateSeries & { hasAnySample: boolean }>;
  threshold: number;
}

/**
 * The differentiator screen: local burn-rate history across days, rendered
 * as small multiples (one chart per account, shared axes) rather than one
 * combined line chart — comparison across accounts is the entire point, and
 * a single chart would hide exactly what this screen exists to show.
 *
 * Fetches its own data rather than receiving it as a prop: `App.tsx` renders
 * this screen with no props, and history/settings are a separate read path
 * from the accounts snapshot `useSnapshot` owns.
 */
export default function HistoryScreen({ settingsThreshold }: { settingsThreshold: number }) {
  const [rangeId, setRangeId] = useState<HistoryRangeId>("30d");
  const [loading, setLoading] = useState(true);
  const [state, setState] = useState<LoadedState | null>(null);

  const days = RANGE_DAYS[rangeId];

  useEffect(() => {
    let cancelled = false;
    setLoading(true);

    (async () => {
      const accountsResult = await getAccounts();
      if (cancelled) return;

      if (!accountsResult.live) {
        // No backend at all: fall back to the same sample fixture the
        // screen always used, flagged by the app-wide sample-data banner
        // (`App.tsx`) rather than a second banner duplicated here.
        const range = mockHistoryRanges.find((r) => r.id === rangeId);
        setState({
          live: false,
          available: true,
          summary: range
            ? {
                weeklyAveragePct: range.stats.weeklyAveragePct,
                timesAt100Pct: range.stats.limitsHit,
                busiestWeekday: range.stats.busiestDay,
              }
            : null,
          series: range
            ? range.series.map((s) => ({ id: s.id, label: s.label, data: s.data, hasAnySample: s.data.length > 0 }))
            : [],
          threshold: settingsThreshold,
        });
        setLoading(false);
        return;
      }

      try {
        const [availableResult, summaryResult] = await Promise.all([historyAvailable(), historySummary(days)]);
        if (cancelled) return;

        if (!availableResult.data) {
          setState({ live: true, available: false, summary: null, series: [], threshold: settingsThreshold });
          setLoading(false);
          return;
        }

        const accounts = accountsResult.data;
        const withKeys = await Promise.all(
          accounts.map(async (account) => ({ account, key: await stableKey(account) })),
        );
        if (cancelled) return;

        const perAccount = await Promise.all(
          withKeys.map(async ({ account, key }) => {
            try {
              const result = await historySeries(key, days);
              return {
                id: key,
                label: displayName(account),
                data: fillDailySeries(result.data, days),
                hasAnySample: result.data.length > 0,
              };
            } catch {
              // One account's history query failing must not blank the
              // whole screen — treat it as "no history yet" for that account.
              return {
                id: key,
                label: displayName(account),
                data: fillDailySeries([], days),
                hasAnySample: false,
              };
            }
          }),
        );
        if (cancelled) return;

        setState({
          live: true,
          available: true,
          summary: summaryResult.data,
          series: perAccount,
          threshold: settingsThreshold,
        });
      } catch {
        if (!cancelled) {
          setState({ live: true, available: false, summary: null, series: [], threshold: settingsThreshold });
        }
      } finally {
        if (!cancelled) setLoading(false);
      }
    })().catch(() => {
      if (!cancelled) setLoading(false);
    });

    return () => {
      cancelled = true;
    };
  }, [rangeId, days, settingsThreshold]);

  const meta = RANGE_META[rangeId];

  const rangeSwitcher = (
    <div className="seg" role="radiogroup" aria-label="History range">
      {RANGE_ORDER.map((id) => (
        <span
          key={id}
          role="radio"
          aria-checked={id === rangeId}
          tabIndex={0}
          className={id === rangeId ? "on" : undefined}
          onClick={() => setRangeId(id)}
          onKeyDown={(e) => {
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              setRangeId(id);
            }
          }}
        >
          {id}
        </span>
      ))}
    </div>
  );

  if (loading && !state) {
    return (
      <div className="pane">
        <div className="pane-head">
          <h3>History</h3>
          {rangeSwitcher}
        </div>
        <span className="sub">Loading…</span>
      </div>
    );
  }

  if (!state) return null;

  const { summary, series, threshold } = state;
  const anySample = series.some((s) => s.hasAnySample);
  const noHistoryYet = state.live && (!state.available || !anySample);

  if (noHistoryYet) {
    return (
      <div className="pane">
        <div className="pane-head">
          <h3>History</h3>
          {rangeSwitcher}
        </div>
        <div className="empty">
          <h3>No history yet</h3>
          <p>
            {state.available
              ? "Usage is recorded from now on — check back after the app has been running a while."
              : "The local history database could not be opened this session, so nothing can be charted right now."}
          </p>
        </div>
      </div>
    );
  }

  return (
    <div className="pane">
      <div className="pane-head">
        <h3>History</h3>
        {rangeSwitcher}
      </div>

      <div className="stat-row">
        <div className="stat">
          <div className="lab">Weekly average</div>
          <div className="val num">{summary ? `${Math.round(summary.weeklyAveragePct)}%` : "—"}</div>
          <div className="foot">across {series.length} account{series.length === 1 ? "" : "s"}</div>
        </div>
        <div className="stat">
          <div className="lab">Limits hit</div>
          <div className="val num">{summary ? summary.timesAt100Pct : "—"}</div>
          <div className="foot">{meta.label}</div>
        </div>
        <div className="stat">
          <div className="lab">Busiest</div>
          <div className="val num">{summary?.busiestWeekday ?? "—"}</div>
          <div className="foot">{meta.label}</div>
        </div>
      </div>

      <hr className="rule" />

      <div className="chart-grid">
        {series.map((s) =>
          s.hasAnySample ? (
            <BurnRateChart key={s.id} series={s} threshold={threshold} startLabel={meta.startLabel} />
          ) : (
            <div className="chart" key={s.id}>
              <div className="chart-head">
                <span className="t">{s.label}</span>
              </div>
              <p style={{ fontSize: 12, color: "var(--muted)", margin: "10px 0" }}>
                No history yet for this account — usage is recorded from now on.
              </p>
            </div>
          ),
        )}
      </div>
    </div>
  );
}
