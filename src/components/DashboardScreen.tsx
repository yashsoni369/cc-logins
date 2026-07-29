import { useCallback, useEffect, useMemo, useState } from "react";

import FleetView from "./dashboard/FleetView";
import AccountAnalytics from "./dashboard/AccountAnalytics";
import { historySamples, historySeries } from "../lib/api";
import { pooledRunway } from "../lib/runway";
import { useNow } from "../lib/time";
import { stableKey, type Account, type DayStat, type Sample, type Snapshot } from "../types";

/** Trailing window the fleet sparklines and the burn estimate are drawn from. */
const FLEET_HOURS = 24;
/** Trailing window the account view charts. Wider than the fleet's, to show more than one 5-hour cycle. */
const ACCOUNT_HOURS = 72;
/** Daily-rollup range for the account view's range chart. */
const ACCOUNT_DAYS = 30;

/**
 * Which of the two views is on screen. The account view is reached by
 * clicking a fleet row and left by its own back control, so it is local
 * state here rather than a route — nothing outside this screen needs to know
 * which account is open, and a nav change would lose the fleet's scroll.
 */
type View = { kind: "fleet" } | { kind: "account"; key: string; number: number };

interface DashboardScreenProps {
  snapshot: Snapshot;
  /** Auto-switch threshold, drawn as a hairline on the account charts. */
  settingsThreshold: number;
  /** True when the most recent background refresh failed; meters dim rather than blank. */
  degraded: boolean;
}

/**
 * The app's home screen: pooled capacity across every account, then one
 * account in depth.
 *
 * Usage percentages come from the snapshot `App` already owns — they are the
 * live reading, and refetching them here would let this screen and the
 * Accounts screen disagree. Only the recorded *history* is fetched here,
 * since that is a separate read path the snapshot does not carry.
 */
export default function DashboardScreen({ snapshot, settingsThreshold, degraded }: DashboardScreenProps) {
  const [view, setView] = useState<View>({ kind: "fleet" });
  const [keyByNumber, setKeyByNumber] = useState<Map<number, string>>(new Map());
  const [samplesByKey, setSamplesByKey] = useState<Map<string, Sample[]>>(new Map());
  const [accountSamples, setAccountSamples] = useState<Sample[]>([]);
  const [accountDaily, setAccountDaily] = useState<DayStat[]>([]);
  const [accountLoading, setAccountLoading] = useState(false);

  const now = useNow();

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

  const keyFor = useCallback(
    (account: Account) => keyByNumber.get(account.number),
    [keyByNumber],
  );

  // Fleet sparklines and the burn estimate. One account's history failing must
  // not blank the fleet, so each settles independently into the map.
  useEffect(() => {
    if (keyByNumber.size === 0) return;
    let cancelled = false;
    void (async () => {
      const entries = await Promise.all(
        [...keyByNumber.values()].map(async (key) => {
          try {
            const result = await historySamples(key, FLEET_HOURS);
            return [key, result.data] as const;
          } catch {
            return [key, [] as Sample[]] as const;
          }
        }),
      );
      if (!cancelled) setSamplesByKey(new Map(entries));
    })();
    return () => {
      cancelled = true;
    };
  }, [keyByNumber]);

  // The open account's deeper history. Only fetched while that view is up.
  useEffect(() => {
    if (view.kind !== "account") return;
    let cancelled = false;
    setAccountLoading(true);
    void (async () => {
      try {
        const [samples, daily] = await Promise.all([
          historySamples(view.key, ACCOUNT_HOURS),
          historySeries(view.key, ACCOUNT_DAYS),
        ]);
        if (cancelled) return;
        setAccountSamples(samples.data);
        setAccountDaily(daily.data);
      } catch {
        // An unreadable history is "nothing recorded yet" for this account,
        // not a failure of the screen — the view says so itself.
        if (!cancelled) {
          setAccountSamples([]);
          setAccountDaily([]);
        }
      } finally {
        if (!cancelled) setAccountLoading(false);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [view]);

  const runway = useMemo(
    () => pooledRunway(accounts, samplesByKey, keyFor, now),
    [accounts, samplesByKey, keyFor, now],
  );

  const openAccount = useCallback((key: string, account: Account) => {
    setAccountSamples([]);
    setAccountDaily([]);
    setView({ kind: "account", key, number: account.number });
  }, []);

  const backToFleet = useCallback(() => setView({ kind: "fleet" }), []);

  if (view.kind === "account") {
    const account = accounts.find((a) => a.number === view.number);
    // The account vanished from the snapshot while its view was open — most
    // likely it was removed elsewhere. Fall back rather than render an empty
    // view of nothing.
    if (!account) {
      return (
        <div className="pane">
          <div className="pane-head">
            <h3>Dashboard</h3>
          </div>
          <div className="empty">
            <h3>That account is gone</h3>
            <p>It was removed while you were looking at it.</p>
            <button type="button" className="btn" onClick={backToFleet}>
              Back to all accounts
            </button>
          </div>
        </div>
      );
    }

    return (
      <div className="pane">
        <AccountAnalytics
          account={account}
          samples={accountSamples}
          daily={accountDaily}
          rangeDays={ACCOUNT_DAYS}
          threshold={settingsThreshold}
          loading={accountLoading}
          onBack={backToFleet}
        />
      </div>
    );
  }

  return (
    <div className="pane">
      <div className="pane-head">
        <h3>Dashboard</h3>
      </div>
      {/* FleetView renders a fragment; this owns the layout and the shared
          column template its header and rows both read. */}
      <div className="fleet">
        <FleetView
          accounts={accounts}
          samplesByKey={samplesByKey}
          keyFor={keyFor}
          runway={runway}
          onOpenAccount={openAccount}
          degraded={degraded}
          now={now}
        />
      </div>
    </div>
  );
}
