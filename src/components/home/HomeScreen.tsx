import { useMemo } from "react";

import { RefreshButton } from "../RefreshButton";
import AccountDrawer, { type DrawerIntent } from "./AccountDrawer";
import AccountsTable, { type RowError } from "./AccountsTable";
import AddAccountMenu, { AddAccountPanel } from "./AddAccountMenu";
import { CoverageKpis, CoverageTimeline } from "./Coverage";
import EnvironmentsPanel from "./EnvironmentsPanel";
import { coveragePlan, moveInOrder, predictTarget, projectLimit, type Projection } from "../../lib/coverage";
import { pooledRunway } from "../../lib/runway";
import { useBurnSamples } from "../../lib/useBurnSamples";
import { ageLabel, type Account, type Settings, type Snapshot } from "../../types";

export interface DrawerState {
  accountNumber: number;
  intent: DrawerIntent;
}

export interface HomeScreenProps {
  snapshot: Snapshot;
  settings: Settings | null;
  now: number;
  /** The most recent background refresh failed. */
  degraded: boolean;
  loginPresent: boolean | undefined;
  /** Which account's drawer is open. Owned by App so the command palette can open it too. */
  drawer: DrawerState | null;
  onDrawerChange: (drawer: DrawerState | null) => void;
  /** Whether the setup-token form is showing. Owned by App for the same reason. */
  showToken: boolean;
  onShowTokenChange: (show: boolean) => void;

  onSwitch: (accountNumber: number) => void;
  pendingAccount: number | null;
  switchError: RowError | null;
  onAddAccount: () => void;
  pendingAddAccount: boolean;
  addAccountError: string | null;
  onAddToken: (token: string, email?: string, alias?: string) => Promise<void>;
  pendingAddToken: boolean;
  addTokenError: string | null;
  onInteractiveLogin: () => void;
  pendingInteractiveLogin: boolean;
  interactiveLoginError: string | null;
  onRelogin: (accountNumber: number) => void;
  pendingReloginAccount: number | null;
  reloginError: RowError | null;
  onSetEnabled: (accountNumber: number, enabled: boolean) => void;
  pendingEnableAccount: number | null;
  enableError: RowError | null;
  onRename: (accountNumber: number, alias: string | null) => Promise<void>;
  onRemove: (accountNumber: number) => Promise<void>;
  onReorder: (order: number[]) => void;
  onWake: (envId: string) => void;
  pendingWake: string | null;
  wakeError: { envId: string; message: string } | null;
  onViewHistory: (accountNumber: number) => void;
  mutationInFlight: boolean;
}

function measuredLabel(accounts: Account[]): string {
  const ages = accounts.map((a) => a.usageAgeSeconds).filter((s): s is number => s != null);
  if (ages.length === 0) return "measured —";
  const age = Math.min(...ages);
  if (age < 60) return `measured ${Math.round(age)}s ago`;
  const label = ageLabel(age);
  return label ? `measured ${label}` : "measured just now";
}

/**
 * Home: the one screen for "now". How long your accounts cover you, the next
 * twelve hours, and every account with its actions. It merges the old
 * Accounts screen with the top of the old Dashboard, so each account is
 * described in exactly one place.
 *
 * Live figures come from the snapshot App owns; only recent local history is
 * read here, to project forward.
 */
export default function HomeScreen(props: HomeScreenProps) {
  const { snapshot, settings, now, degraded, drawer, onDrawerChange: setDrawer, showToken, onShowTokenChange: setShowToken } = props;
  const accounts = useMemo(() => snapshot.environments.flatMap((e) => e.accounts), [snapshot]);
  const { keyFor, burnByKey, activeSamples } = useBurnSamples(accounts, snapshot);

  const autoSwitch = settings?.autoSwitchEnabled ?? false;
  const threshold = settings?.threshold ?? 90;
  const strategy = settings?.strategy ?? "most-headroom";

  const runway = useMemo(() => pooledRunway(accounts, burnByKey, keyFor, now), [accounts, burnByKey, keyFor, now]);
  const active = accounts.find((a) => a.active) ?? null;
  const activeProjection = useMemo(() => (active ? projectLimit(active, activeSamples, now) : null), [active, activeSamples, now]);
  const bestNext = useMemo(() => predictTarget(accounts, strategy, now), [accounts, strategy, now]);
  const plan = useMemo(
    () => coveragePlan({ accounts, activeSamples, now, horizonHours: 12, autoSwitch, threshold, strategy }),
    [accounts, activeSamples, now, autoSwitch, threshold, strategy],
  );
  const projections = useMemo(() => {
    const map = new Map<number, Projection>();
    if (active && activeProjection) map.set(active.number, activeProjection);
    return map;
  }, [active, activeProjection]);

  // A removed account closes its drawer instead of describing something gone.
  const drawerAccount = drawer ? (accounts.find((a) => a.number === drawer.accountNumber) ?? null) : null;

  const busyLabel = props.pendingAddAccount
    ? "Adding…"
    : props.pendingInteractiveLogin
      ? "Waiting for sign-in…"
      : props.pendingAddToken
        ? "Adding…"
        : null;

  return (
    <div className="pane home">
      <div className="pane-head">
        <h3>Home</h3>
        <span className="sub num">{measuredLabel(accounts)}</span>
        <span className="sp" />
        <RefreshButton />
        <AddAccountMenu
          loginPresent={props.loginPresent}
          onAddCurrent={props.onAddAccount}
          onSignIn={props.onInteractiveLogin}
          onPasteToken={() => setShowToken(true)}
          busyLabel={busyLabel}
          disabled={props.mutationInFlight}
        />
      </div>

      <AddAccountPanel
        pendingSignIn={props.pendingInteractiveLogin}
        signInError={props.interactiveLoginError}
        addCurrentError={props.addAccountError}
        showToken={showToken}
        onCloseToken={() => setShowToken(false)}
        onAddToken={props.onAddToken}
        pendingAddToken={props.pendingAddToken}
        addTokenError={props.addTokenError}
      />

      <CoverageKpis
        accounts={accounts}
        runway={runway}
        activeProjection={activeProjection}
        bestNext={bestNext}
        autoSwitch={autoSwitch}
        degraded={degraded}
        now={now}
      />

      <CoverageTimeline plan={plan} now={now} autoSwitch={autoSwitch} degraded={degraded} />

      <section className="band" aria-label="Accounts">
        <div className="band-head">
          <h2>Accounts</h2>
          <span className="sub">click a row for details</span>
        </div>
        <AccountsTable
          accounts={accounts}
          now={now}
          projections={projections}
          bestNext={bestNext?.number ?? null}
          onOpen={(n) => setDrawer({ accountNumber: n, intent: "view" })}
          onSwitch={props.onSwitch}
          pendingAccount={props.pendingAccount}
          switchError={props.switchError}
          onRelogin={props.onRelogin}
          pendingReloginAccount={props.pendingReloginAccount}
          reloginError={props.reloginError}
          onSetEnabled={props.onSetEnabled}
          pendingEnableAccount={props.pendingEnableAccount}
          enableError={props.enableError}
          onMove={(n, direction) => props.onReorder(moveInOrder(accounts, n, direction))}
          onRename={(n) => setDrawer({ accountNumber: n, intent: "rename" })}
          onRemove={(n) => setDrawer({ accountNumber: n, intent: "remove" })}
          mutationInFlight={props.mutationInFlight}
          degraded={degraded}
        />
      </section>

      <EnvironmentsPanel
        environments={snapshot.environments}
        onWake={props.onWake}
        pendingWake={props.pendingWake}
        wakeError={props.wakeError}
        mutationInFlight={props.mutationInFlight}
      />

      <AccountDrawer
        account={drawerAccount}
        intent={drawer?.intent ?? "view"}
        now={now}
        projection={drawerAccount?.active ? activeProjection : null}
        onClose={() => setDrawer(null)}
        onSwitch={props.onSwitch}
        onRelogin={props.onRelogin}
        onSetEnabled={props.onSetEnabled}
        onRename={props.onRename}
        onRemove={async (n) => {
          await props.onRemove(n);
          setDrawer(null);
        }}
        onViewHistory={(n) => {
          setDrawer(null);
          props.onViewHistory(n);
        }}
        mutationInFlight={props.mutationInFlight}
      />
    </div>
  );
}
