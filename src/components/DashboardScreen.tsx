import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import AccountRow, { type AccountDetail } from "./dashboard/AccountRow";
import Insights from "./dashboard/Insights";
import LoadBalance from "./dashboard/LoadBalance";
import RangePicker from "./dashboard/RangePicker";
import RotationChart from "./dashboard/RotationChart";
import { Loading } from "./Loading";
import { historySamples, historySeries } from "../lib/api";
import {
  RANGES,
  buildFleetSeries,
  deriveInsights,
  loadBalanceGrid,
  type RangeKey,
} from "../lib/dashboard";
import { useNow } from "../lib/time";
import { stableKey, type Account, type DayStat, type Sample, type Snapshot } from "../types";

/** Intraday window an opened account charts. Wider than one 5-hour cycle. */
const DETAIL_HOURS = 72;
/** Daily-rollup range an opened account charts. */
const DETAIL_DAYS = 30;
/**
 * Days of daily history fetched for the load-balance grid.
 *
 * Deliberately more than fits on a normal window: the grid keeps its cells a
 * constant size and shows as many days as the pane can hold, so a wider window
 * buys more history instead of fatter blocks. This is the ceiling on that.
 *
 * It is not tied to the selected range. Tying it made 24h render seven cells
 * and "All" render 365 — one too coarse to be a pattern, the other unreadable.
 */
const BALANCE_DAYS = 180;
/** Span the derived findings reason over, whatever the grid happens to show. */
const INSIGHT_DAYS = 30;

interface DashboardScreenProps {
  snapshot: Snapshot;
  /** Auto-switch threshold, drawn as a hairline on the charts. */
  settingsThreshold: number;
  /** True when the most recent background refresh failed; meters dim rather than blank. */
  degraded: boolean;
  /** Opens one account in place on arrival — Home's "View history". `nonce` makes a repeat request count. */
  focus?: { accountNumber: number; nonce: number } | null;
}

/** Settle every promise independently: one account's unreadable history must
 *  not blank the fleet, so a failure becomes an empty series for that key. */
async function gather<T>(
  keys: string[],
  load: (key: string) => Promise<{ data: T[] }>,
): Promise<Map<string, T[]>> {
  const entries = await Promise.all(
    keys.map(async (key) => {
      try {
        return [key, (await load(key)).data] as const;
      } catch {
        return [key, [] as T[]] as const;
      }
    }),
  );
  return new Map(entries);
}

/**
 * History: what your own recorded usage says, utilisation over time, and any
 * account opened in place beneath its own row. "Now" lives on Home.
 *
 * Live usage percentages come from the snapshot `App` already owns — they are
 * the authoritative reading, and refetching them here would let this screen
 * and the Accounts screen disagree. Only recorded *history* is fetched here.
 */
export default function DashboardScreen({ snapshot, settingsThreshold, degraded, focus = null }: DashboardScreenProps) {
  const [range, setRange] = useState<RangeKey>("7d");
  const [expanded, setExpanded] = useState<Set<number>>(new Set());
  const [keyByNumber, setKeyByNumber] = useState<Map<number, string>>(new Map());

  const [rangeSamples, setRangeSamples] = useState<Map<string, Sample[]>>(new Map());
  const [rangeDaily, setRangeDaily] = useState<Map<string, DayStat[]>>(new Map());
  // Distinguishes "still reading" from "nothing recorded". Without it the
  // rotation band announced an empty history for the moment before its data
  // arrived, which is a lie the reader has no way to spot.
  const [rangeLoading, setRangeLoading] = useState(true);
  const [detailByKey, setDetailByKey] = useState<Map<string, AccountDetail>>(new Map());

  const now = useNow();
  const spec = RANGES[range];

  const accounts = useMemo(
    () => snapshot.environments.flatMap((environment) => environment.accounts),
    [snapshot],
  );

  // `stableKey` is async (it hashes the email via crypto.subtle), so keys are
  // resolved once into a lookup rather than awaited at every render.
  useEffect(() => {
    let cancelled = false;
    void (async () => {
      const pairs = await Promise.all(
        accounts.map(async (account) => [account.number, await stableKey(account)] as const),
      );
      if (!cancelled) setKeyByNumber(new Map(pairs));
    })();
    return () => {
      cancelled = true;
    };
  }, [accounts]);

  const keyFor = useCallback((account: Account) => keyByNumber.get(account.number), [keyByNumber]);
  const keys = useMemo(() => [...keyByNumber.values()], [keyByNumber]);

  // The burn estimate's own fixed window, refetched only as accounts change.
  useEffect(() => {
    if (keys.length === 0) return;
    let cancelled = false;
    setRangeLoading(true);

    void (async () => {
      // The grid's span is fixed, so ask for whichever is longer and let each
      // consumer slice what it needs from one read.
      const daily = await gather(keys, (key) => historySeries(key, Math.max(spec.days, BALANCE_DAYS)));
      if (!cancelled) setRangeDaily(daily);

      // "auto" needs the samples in hand to judge whether they are drawable,
      // so it reads them too and may then ignore them.
      if (spec.source !== "daily") {
        const samples = await gather(keys, (key) => historySamples(key, spec.hours));
        if (!cancelled) setRangeSamples(samples);
      } else if (!cancelled) {
        setRangeSamples(new Map());
      }
      if (!cancelled) setRangeLoading(false);
    })();

    return () => {
      cancelled = true;
    };
  }, [keys, spec]);

  /**
   * Load an opened account's deeper history, once.
   *
   * Deliberately not an effect. Opening a row is a user event, and an effect
   * keyed on the expanded set would have to read the cache it also writes —
   * which re-runs on its own output. The ref tracks what has been requested so
   * a double-click cannot start two fetches for one account.
   */
  const requested = useRef<Set<string>>(new Set());
  const loadDetail = useCallback(async (key: string) => {
    if (requested.current.has(key)) return;
    requested.current.add(key);
    setDetailByKey((prev) => new Map(prev).set(key, { samples: [], daily: [], loading: true }));

    const [samples, daily] = await Promise.all([
      historySamples(key, DETAIL_HOURS).then((r) => r.data).catch(() => [] as Sample[]),
      historySeries(key, DETAIL_DAYS).then((r) => r.data).catch(() => [] as DayStat[]),
    ]);
    if (!mounted.current) return;
    setDetailByKey((prev) => new Map(prev).set(key, { samples, daily, loading: false }));
  }, []);

  // Accounts can disappear while their history is in flight; without this the
  // late resolve would set state on an unmounted screen.
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  // A removed account must not keep a stale drawer or a cached fetch alive.
  useEffect(() => {
    const live = new Set(accounts.map((a) => a.number));
    setExpanded((prev) => {
      const next = new Set([...prev].filter((n) => live.has(n)));
      return next.size === prev.size ? prev : next;
    });
  }, [accounts]);

  const series = useMemo(
    () => buildFleetSeries(accounts, keyFor, rangeSamples, rangeDaily, spec, 240, now),
    [accounts, keyFor, rangeSamples, rangeDaily, spec, now],
  );

  const loadRows = useMemo(
    () => loadBalanceGrid(accounts, keyFor, rangeDaily, BALANCE_DAYS, now),
    [accounts, keyFor, rangeDaily, now],
  );

  // Findings reason over a fixed month; the grid above may be showing far more
  // or less depending on how wide the window is, and a finding that quietly
  // changed span with the window size would be unreadable as a claim.
  const insightRows = useMemo(
    () => loadRows.map((row) => ({ ...row, cells: row.cells.slice(-INSIGHT_DAYS) })),
    [loadRows],
  );

  const insights = useMemo(
    () => deriveInsights({ accounts, series, rows: insightRows, spec, threshold: settingsThreshold, now }),
    [accounts, series, insightRows, spec, settingsThreshold, now],
  );

  const toggle = useCallback(
    (number: number) => {
      setExpanded((prev) => {
        const next = new Set(prev);
        if (next.has(number)) next.delete(number);
        else next.add(number);
        return next;
      });
      const key = keyByNumber.get(number);
      if (key) void loadDetail(key);
    },
    [keyByNumber, loadDetail],
  );

  // Arriving from Home's "View history": open that account in place once its
  // history key has resolved. The key resolves asynchronously, so this is a
  // genuine synchronisation with an external result, not derived state.
  const handledFocus = useRef<number | null>(null);
  useEffect(() => {
    if (!focus || handledFocus.current === focus.nonce) return;
    const key = keyByNumber.get(focus.accountNumber);
    if (!key) return;
    handledFocus.current = focus.nonce;
    setExpanded((prev) => new Set(prev).add(focus.accountNumber));
    void loadDetail(key);
    requestAnimationFrame(() =>
      document.getElementById(`history-row-${focus.accountNumber}`)?.scrollIntoView({ block: "start" }),
    );
  }, [focus, keyByNumber, loadDetail]);

  if (accounts.length === 0) {
    return (
      <div className="pane">
        <div className="pane-head">
          <h3>History</h3>
        </div>
        <div className="empty">
          <h3>No accounts yet</h3>
          <p>Add an account and this fills in as usage is recorded.</p>
        </div>
      </div>
    );
  }

  return (
    <div className="pane dash">
      <div className="pane-head">
        <h3>History</h3>
        <RangePicker value={range} onChange={setRange} />
      </div>

      <Insights items={insights} />

      <section className="band">
        <div className="band-head">
          <h2>Utilisation history</h2>
          <span className="sub">limiting window · {spec.phrase}</span>
          <span className="spacer" />
          <span className="sub">hover a line to isolate it</span>
        </div>
        {rangeLoading && series.every((s) => s.runs.length === 0) ? (
          <Loading label="Reading recorded history" />
        ) : (
          <RotationChart series={series} spec={spec} threshold={settingsThreshold} />
        )}
      </section>

      <section className="band">
        <div className="band-head">
          <h2>Accounts</h2>
          <span className="sub">click to open in place</span>
        </div>
        <div className="rows">
          {accounts.map((account) => {
            const key = keyFor(account);
            return (
              <div key={account.number} id={`history-row-${account.number}`} className="history-row">
                <AccountRow
                  account={account}
                  detail={key ? detailByKey.get(key) : undefined}
                  series={series.find((s) => s.number === account.number)}
                  expanded={expanded.has(account.number)}
                  onToggle={() => toggle(account.number)}
                  threshold={settingsThreshold}
                  rangeDays={DETAIL_DAYS}
                  peers={accounts.filter((a) => a.number !== account.number)}
                  degraded={degraded}
                  now={now}
                />
              </div>
            );
          })}
        </div>
      </section>

      <LoadBalance rows={loadRows} />
    </div>
  );
}
