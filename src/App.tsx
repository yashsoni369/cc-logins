import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from "react";
import DashboardScreen from "./components/DashboardScreen";
import SettingsScreen from "./components/SettingsScreen";
import FirstRunScreen from "./components/FirstRunScreen";
import HomeScreen, { type DrawerState } from "./components/home/HomeScreen";
import type { PaletteCommand } from "./components/CommandPalette";
import Toggle from "./components/Toggle";
import { ToastProvider, useToast } from "./components/ui/ToastRegion";
import { LoadingPane } from "./components/Loading";
import { ClockFormatProvider } from "./lib/clockFormat";
import { DisplayModeProvider } from "./lib/displayMode";
import { predictTarget } from "./lib/coverage";
import { useQuotaNotifications } from "./lib/notifications";
import { useSnapshot } from "./lib/useSnapshot";
import { useTheme } from "./lib/useTheme";
import { useSettings } from "./lib/useSettings";
import { useDaemonStatus } from "./lib/useDaemonStatus";
import { useUpdate } from "./lib/useUpdate";
import { formatCountdown, useNow } from "./lib/time";
import {
  addCurrentAccount,
  addToken,
  hasBackend,
  interactiveLogin,
  IpcError,
  refreshSnapshot,
  reloginAccount,
  removeAccount,
  reorderAccounts,
  setAccountAlias,
  setAccountEnabled,
  switchAccount,
  wakeEnvironment,
} from "./lib/api";
import { bindingUtilisation, bindingWindow, displayName, type DaemonPhase, type Snapshot } from "./types";

// Loaded the first time the palette opens, so `cmdk` stays out of the startup bundle.
const CommandPalette = lazy(() => import("./components/CommandPalette"));

type Screen = "home" | "history" | "settings";

const NAV_ITEMS: Array<{ id: Screen; label: string }> = [
  { id: "home", label: "Home" },
  { id: "history", label: "History" },
  { id: "settings", label: "Settings" },
];

/** ⌘ on macOS, Ctrl elsewhere — for the palette hint. */
const MOD_KEY = typeof navigator !== "undefined" && /Mac|iPhone|iPad/i.test(navigator.userAgent) ? "⌘" : "Ctrl";

/** Per-account outcome of the most recent switch attempt. */
interface SwitchError {
  accountNumber: number;
  message: string;
}

/** Per-account outcome of the most recent enable/disable attempt. */
interface EnableError {
  accountNumber: number;
  message: string;
}

/** Shared across every mutation below: the backend's single credential lock is held elsewhere. */
const BUSY_MESSAGE =
  "Another process is using your accounts right now. Try again in a moment.";

export function daemonPhaseLabel(phase: DaemonPhase | undefined, strategy: string | undefined): string {
  const strategyLabel =
    strategy === "next-available"
      ? "next"
      : strategy === "consume-first"
        ? "consume first"
        : "best";
  switch (phase?.kind) {
    case "disabled":
      return "Off";
    case "paused":
      return "Paused";
    case "cooldown":
      return "Cooldown";
    case "warning":
      return "Switch pending";
    case "switching":
      return "Switching";
    case "exhausted":
      return "No account available";
    case "degraded":
      return "Degraded";
    case "recoveryRequired":
      return "Recovery required";
    case "monitoring":
      return `Running · ${strategyLabel}`;
    default:
      return "Loading…";
  }
}

/** Relaunches the app; recovery is retried on start. Permission `process:allow-restart` is already granted. */
async function restartApp(): Promise<void> {
  const { relaunch } = await import("@tauri-apps/plugin-process");
  await relaunch();
}

export function MainRecoveryBanner({ phase }: { phase: Extract<DaemonPhase, { kind: "recoveryRequired" }> }) {
  return (
    <div className="banner danger main-daemon-banner" role="alert">
      <div>
        <b>Recovery required</b>
        <div>{phase.detail}</div>
        <div>Account changes are disabled until recovery succeeds. Restart the app to retry recovery.</div>
      </div>
      {hasBackend() && (
        <button type="button" className="btn" onClick={() => void restartApp()}>
          Restart now
        </button>
      )}
    </div>
  );
}

/** Message for a failed "Add account" — worded specifically for the one failure users will actually hit. */
function describeAddAccountError(err: unknown): string {
  if (err instanceof IpcError) {
    if (err.isBusy) return BUSY_MESSAGE;
    if (err.isAlreadyRegistered) {
      return "This login is already registered as an account here — nothing to add.";
    }
    return err.detail ?? err.message;
  }
  return err instanceof Error ? err.message : "Couldn't add this account.";
}

/**
 * Message for a failed `interactiveLogin`, per the outcomes the interactive
 * sign-in contract distinguishes. Returns `null` for a cancellation — the
 * user just closed the terminal, which is not an error, so the caller must
 * render nothing rather than an alarming banner.
 */
export function describeInteractiveLoginError(err: unknown): string | null {
  if (err instanceof IpcError) {
    if (err.isBusy) return BUSY_MESSAGE;
    if (err.isCancelled) {
      return null;
    }
    if (err.isTimedOut) {
      return "Timed out waiting for sign-in. Nothing was added — try again when you're ready.";
    }
    if (err.isPrerequisiteMissing) {
      // The backend names the directories it actually searched, and how to
      // point the app at a custom install — far more actionable than anything
      // this side could guess. Fall back only if `detail` is somehow absent.
      return (
        err.detail ??
        "Claude Code isn't installed, or the `claude` command isn't on PATH. Install it, then try again."
      );
    }
    if (err.isNoTerminalAvailable) {
      return 'Couldn\'t open a terminal on this system. Use "Add token" below instead.';
    }
    if (err.isAlreadyRegistered) {
      return "That account is already registered here — nothing to add.";
    }
    return err.detail ?? err.message;
  }
  return err instanceof Error ? err.message : "Couldn't sign in to a new account.";
}

/** Message for a failed "Add token". */
function describeAddTokenError(err: unknown): string {
  if (err instanceof IpcError) {
    if (err.isBusy) return BUSY_MESSAGE;
    return err.detail ?? err.message;
  }
  return err instanceof Error ? err.message : "Couldn't add this token.";
}

/** Message for a failed enable/disable — worded specifically when the backend refused to disable the active account. */
function describeEnableError(err: unknown, enabled: boolean): string {
  if (err instanceof IpcError) {
    if (err.isBusy) return BUSY_MESSAGE;
    if (!enabled && err.isCannotDisableActive) {
      return "This is the account currently in use — switch to another account before disabling it.";
    }
    return err.detail ?? err.message;
  }
  return err instanceof Error ? err.message : `Couldn't ${enabled ? "enable" : "disable"} this account.`;
}

/**
 * Persistent, non-dismissible notice that the screen is showing `mock.ts`
 * sample data rather than the user's real accounts. Only ever rendered when
 * `live === false`; there is no close button, because a stale banner beats
 * fiction that looks like someone's real quota.
 */
function SampleDataBanner() {
  return (
    <div className="sample-banner" role="status">
      Sample data — these are not your real accounts or usage.
    </div>
  );
}

/** Message for a failed rename, reorder or remove. */
function describeAccountEditError(err: unknown, fallback: string): string {
  if (err instanceof IpcError) {
    if (err.isBusy) return BUSY_MESSAGE;
    if (err.isCannotRemoveActive) {
      return "This is the account currently in use — switch to another account before removing it.";
    }
    return err.detail ?? err.message;
  }
  return err instanceof Error ? err.message : fallback;
}

/** "Next: Personal ~5:05 PM"-style hint for the sidebar, or null when nothing is predictable. */
function nextHint(snapshot: Snapshot, phase: DaemonPhase | undefined, strategy: string | undefined, now: number): string | null {
  const accounts = snapshot.environments.flatMap((e) => e.accounts);
  if (phase?.kind === "warning") {
    const to = accounts.find((a) => a.number === phase.to);
    return to ? `Switching to ${displayName(to)}` : null;
  }
  if (phase?.kind !== "monitoring" && phase?.kind !== "cooldown") return null;
  const target = predictTarget(
    accounts,
    strategy === "next-available" || strategy === "consume-first" ? strategy : "most-headroom",
    now,
  );
  return target ? `Next: ${displayName(target)}` : "No other account ready";
}

export default function App() {
  return (
    <ToastProvider>
      <AppContent />
    </ToastProvider>
  );
}

function AppContent() {
  const [screen, setScreen] = useState<Screen>("home");
  const { snapshot, live, loading, error, refresh } = useSnapshot();
  const settings = useSettings();
  const daemon = useDaemonStatus();
  const toast = useToast();
  const now = useNow();
  // Mounted here and nowhere else: a second scheduler in the popover window
  // would double every check and could announce the same release twice.
  const update = useUpdate(
    settings.settings?.autoCheckUpdates ?? false,
    daemon.status?.phase ?? null,
  );
  // Applies the persisted theme to this window's <html> and keeps it live
  // against OS changes. SettingsScreen gets the same instance as props
  // rather than mounting its own, so the segmented control there and the
  // theme actually applied to this document never disagree.
  const theme = useTheme(settings);
  const clockFormat = settings.settings?.clockFormat ?? "system";

  const [pendingAccount, setPendingAccount] = useState<number | null>(null);
  const [switchError, setSwitchError] = useState<SwitchError | null>(null);
  const [pendingAddAccount, setPendingAddAccount] = useState(false);
  const [addAccountError, setAddAccountError] = useState<string | null>(null);
  const [pendingInteractiveLogin, setPendingInteractiveLogin] = useState(false);
  const [interactiveLoginError, setInteractiveLoginError] = useState<string | null>(null);
  const [pendingReloginAccount, setPendingReloginAccount] = useState<number | null>(null);
  const [reloginError, setReloginError] = useState<SwitchError | null>(null);
  const [pendingAddToken, setPendingAddToken] = useState(false);
  const [addTokenError, setAddTokenError] = useState<string | null>(null);
  const [pendingEnableAccount, setPendingEnableAccount] = useState<number | null>(null);
  const [enableError, setEnableError] = useState<EnableError | null>(null);
  const [pendingEdit, setPendingEdit] = useState(false);
  const [pendingWake, setPendingWake] = useState<string | null>(null);
  const [wakeError, setWakeError] = useState<{ envId: string; message: string } | null>(null);
  const [autoSwitchError, setAutoSwitchError] = useState<string | null>(null);

  // UI state other surfaces (the palette) can drive, so it lives here.
  const [drawer, setDrawer] = useState<DrawerState | null>(null);
  const [showToken, setShowToken] = useState(false);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [historyFocus, setHistoryFocus] = useState<{ accountNumber: number; nonce: number } | null>(null);

  // Every mutation below returns the post-change Snapshot, which we show
  // immediately rather than refetching or guessing. It stands in for the
  // poller's own `snapshot` only until that poller's next tick lands (see the
  // effect below), at which point the real, freshly-polled data takes back
  // over.
  const [snapshotOverride, setSnapshotOverride] = useState<Snapshot | null>(null);
  useEffect(() => {
    setSnapshotOverride(null);
  }, [snapshot]);

  const daemonPhase = daemon.status?.phase;
  const displaySnapshot: Snapshot | null = snapshotOverride ?? snapshot;
  useQuotaNotifications(displaySnapshot, daemonPhase, settings.settings, clockFormat);

  // True while any mutating call is in flight. All mutating buttons disable
  // together: they all touch the same single-writer credential store the
  // backend guards with its "busy" lock, so nothing is gained by letting two
  // race to hit it at once.
  const recoveryBlocked = daemonPhase?.kind === "recoveryRequired";
  const mutationInFlight =
    recoveryBlocked ||
    pendingAccount !== null ||
    pendingAddAccount ||
    pendingAddToken ||
    pendingInteractiveLogin ||
    pendingReloginAccount !== null ||
    pendingEnableAccount !== null ||
    pendingEdit ||
    pendingWake !== null;

  const accountsNow = useMemo(() => displaySnapshot?.environments.flatMap((e) => e.accounts) ?? [], [displaySnapshot]);

  // The ONLY call site for the mutating `switchAccount`. Reached from a click
  // (Home, the drawer, the palette, or Undo) — never from an effect, a timer,
  // or on mount.
  const handleSwitch = useCallback(
    (accountNumber: number, options?: { undoable?: boolean }) => {
      const previous = accountsNow.find((a) => a.active) ?? null;
      const target = accountsNow.find((a) => a.number === accountNumber) ?? null;
      setPendingAccount(accountNumber);
      setSwitchError(null);
      switchAccount(accountNumber)
        .then((result) => {
          setSnapshotOverride(result);
          void refresh();
          if (options?.undoable !== false && previous && target) {
            toast.show({
              message: `Switched to ${displayName(target)}`,
              action: {
                label: `Back to ${displayName(previous)}`,
                run: () => switchRef.current(previous.number, { undoable: false }),
              },
            });
          }
        })
        .catch((err: unknown) => {
          const message =
            err instanceof IpcError && err.isBusy
              ? BUSY_MESSAGE
              : err instanceof IpcError && err.isReloginRequired
                ? "This account needs a fresh sign-in before it can be activated. Sign in again and re-add it."
              : err instanceof Error
                ? err.message
                : "Couldn't switch accounts.";
          setSwitchError({ accountNumber, message });
        })
        .finally(() => setPendingAccount(null));
    },
    [accountsNow, refresh, toast],
  );
  // Undo re-enters the same handler. The ref keeps the toast's action pointing
  // at the latest version without the callback depending on itself.
  const switchRef = useRef(handleSwitch);
  useEffect(() => {
    switchRef.current = handleSwitch;
  }, [handleSwitch]);

  // The ONLY call site for `addCurrentAccount`.
  const handleAddAccount = useCallback(() => {
    setPendingAddAccount(true);
    setAddAccountError(null);
    addCurrentAccount()
      .then((result) => setSnapshotOverride(result))
      .catch((err: unknown) => setAddAccountError(describeAddAccountError(err)))
      .finally(() => setPendingAddAccount(false));
  }, []);

  // The ONLY call site for `interactiveLogin`.
  const handleInteractiveLogin = useCallback(() => {
    setPendingInteractiveLogin(true);
    setInteractiveLoginError(null);
    interactiveLogin()
      .then((result) => setSnapshotOverride(result))
      .catch((err: unknown) => {
        // `null` means the user cancelled by closing the terminal — quiet,
        // not an error, so nothing is set and the flow returns to rest.
        const message = describeInteractiveLoginError(err);
        if (message !== null) setInteractiveLoginError(message);
      })
      .finally(() => setPendingInteractiveLogin(false));
  }, []);

  const handleRelogin = useCallback((accountNumber: number) => {
    setPendingReloginAccount(accountNumber);
    setReloginError(null);
    reloginAccount(accountNumber)
      .then((result) => setSnapshotOverride(result))
      .catch((err: unknown) => {
        const message = describeInteractiveLoginError(err);
        if (message !== null) setReloginError({ accountNumber, message });
      })
      .finally(() => setPendingReloginAccount(null));
  }, []);

  // The ONLY call site for `addToken`, reached from the token form's submit.
  const handleAddToken = useCallback(async (token: string, email?: string, alias?: string) => {
    setPendingAddToken(true);
    setAddTokenError(null);
    try {
      const result = await addToken(token, email, alias);
      setSnapshotOverride(result);
    } catch (err) {
      setAddTokenError(describeAddTokenError(err));
      throw err; // lets the form know not to close itself
    } finally {
      setPendingAddToken(false);
    }
  }, []);

  // The ONLY call site for `setAccountEnabled`.
  const handleSetEnabled = useCallback((accountNumber: number, enabled: boolean) => {
    setPendingEnableAccount(accountNumber);
    setEnableError(null);
    setAccountEnabled(accountNumber, enabled)
      .then((result) => setSnapshotOverride(result))
      .catch((err: unknown) => setEnableError({ accountNumber, message: describeEnableError(err, enabled) }))
      .finally(() => setPendingEnableAccount(null));
  }, []);

  /** Rename, remove: resolve on success, reject with a readable Error the drawer shows. */
  const runEdit = useCallback(async (edit: () => Promise<Snapshot>, fallback: string) => {
    setPendingEdit(true);
    try {
      setSnapshotOverride(await edit());
    } catch (err) {
      throw new Error(describeAccountEditError(err, fallback));
    } finally {
      setPendingEdit(false);
    }
  }, []);

  const handleRename = useCallback(
    (accountNumber: number, alias: string | null) => runEdit(() => setAccountAlias(accountNumber, alias), "Couldn't rename this account."),
    [runEdit],
  );

  const handleRemove = useCallback(
    async (accountNumber: number) => {
      const name = accountsNow.find((a) => a.number === accountNumber);
      await runEdit(() => removeAccount(accountNumber), "Couldn't remove this account.");
      toast.show({ message: `Removed ${name ? displayName(name) : "the account"}` });
    },
    [accountsNow, runEdit, toast],
  );

  const handleReorder = useCallback(
    (order: number[]) => {
      runEdit(() => reorderAccounts(order), "Couldn't reorder accounts.").catch((err: unknown) => {
        toast.show({ message: err instanceof Error ? err.message : "Couldn't reorder accounts." });
      });
    },
    [runEdit, toast],
  );

  // The ONLY call site for `wakeEnvironment` — it boots a Linux VM, so only an explicit click reaches it.
  const handleWake = useCallback((envId: string) => {
    setPendingWake(envId);
    setWakeError(null);
    wakeEnvironment(envId)
      .then((result) => setSnapshotOverride(result))
      .catch((err: unknown) =>
        setWakeError({ envId, message: err instanceof IpcError ? (err.detail ?? err.message) : "Couldn't wake this distro." }),
      )
      .finally(() => setPendingWake(null));
  }, []);

  const setAutoSwitch = useCallback(
    (enabled: boolean) => {
      setAutoSwitchError(null);
      settings.update({ autoSwitchEnabled: enabled }).catch((err: unknown) => {
        setAutoSwitchError(err instanceof Error ? err.message : "Couldn't change auto-switch.");
      });
    },
    [settings.update],
  );

  const openHistoryFor = useCallback((accountNumber: number) => {
    setHistoryFocus((prev) => ({ accountNumber, nonce: (prev?.nonce ?? 0) + 1 }));
    setScreen("history");
  }, []);

  // ⌘K / Ctrl+K anywhere in the window.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setPaletteOpen((open) => !open);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const phaseKind = daemonPhase?.kind;
  const commands = useMemo<PaletteCommand[]>(() => {
    const list: PaletteCommand[] = [];
    for (const account of accountsNow) {
      const unavailable =
        account.active || account.usageStatus === "disabled" || account.usageStatus === "reloginrequired";
      if (unavailable || mutationInFlight) continue;
      const pct = bindingUtilisation(account.usage);
      const reset = formatCountdown(bindingWindow(account.usage)?.resetsAt, now);
      list.push({
        id: `switch-${account.number}`,
        group: "Switch to",
        label: displayName(account),
        meta: [pct === null ? null : `${Math.round(pct)}%`, reset ? `resets ${reset}` : null].filter(Boolean).join(" · "),
        keywords: [account.email, account.organizationName ?? ""],
        run: () => handleSwitch(account.number),
      });
    }
    for (const item of NAV_ITEMS) {
      list.push({ id: `go-${item.id}`, group: "Go to", label: item.label, run: () => setScreen(item.id) });
    }
    for (const account of accountsNow) {
      list.push({
        id: `details-${account.number}`,
        group: "Go to",
        label: `${displayName(account)} details`,
        run: () => {
          setScreen("home");
          setDrawer({ accountNumber: account.number, intent: "view" });
        },
      });
    }
    if (hasBackend()) {
      list.push({ id: "refresh", group: "Actions", label: "Refresh usage now", run: () => void refreshSnapshot().catch(() => {}) });
    }
    if (phaseKind === "disabled") {
      list.push({ id: "auto-on", group: "Actions", label: "Turn auto-switch on", run: () => setAutoSwitch(true) });
    } else if (phaseKind === "paused") {
      list.push({ id: "auto-resume", group: "Actions", label: "Resume auto-switch", run: () => void settings.resume().catch(() => {}) });
    } else if (phaseKind) {
      list.push({ id: "auto-hold", group: "Actions", label: "Hold auto-switch for 1 hour", run: () => void settings.snooze(3600).catch(() => {}) });
      list.push({ id: "auto-off", group: "Actions", label: "Turn auto-switch off", run: () => setAutoSwitch(false) });
    }
    if (!mutationInFlight) {
      list.push({ id: "add-current", group: "Actions", label: "Add the account Claude Code is signed into", run: handleAddAccount });
      list.push({ id: "add-signin", group: "Actions", label: "Sign in to another account", run: handleInteractiveLogin });
      list.push({
        id: "add-token",
        group: "Actions",
        label: "Paste a setup token or API key",
        run: () => {
          setScreen("home");
          setShowToken(true);
        },
      });
    }
    list.push({ id: "theme-day", group: "Actions", label: "Theme: Day", run: () => theme.setTheme("day") });
    list.push({ id: "theme-night", group: "Actions", label: "Theme: Night", run: () => theme.setTheme("night") });
    list.push({ id: "theme-system", group: "Actions", label: "Theme: match system", run: () => theme.setTheme("system") });
    return list;
  }, [accountsNow, mutationInFlight, now, handleSwitch, phaseKind, setAutoSwitch, settings, handleAddAccount, handleInteractiveLogin, theme]);

  // An unconfigured machine is a normal state, not an error — it routes to
  // the same first-run screen as "zero accounts found" below.
  const notConfigured = error instanceof IpcError && error.isNotConfigured;

  if (notConfigured) {
    return (
      <div className="win">
        <FirstRunScreen
          onAction={(action) => (action === "signIn" ? handleInteractiveLogin() : handleAddAccount())}
          pending={pendingInteractiveLogin ? "signIn" : pendingAddAccount ? "addCurrent" : null}
          error={interactiveLoginError ?? addAccountError}
        />
      </div>
    );
  }

  if (loading && !snapshot) {
    return (
      <div className="win">
        {/* `.loading-pane` fills and centres itself, so the pane needs no inline centring. */}
        <div className="pane">
          <LoadingPane />
        </div>
      </div>
    );
  }

  if (!displaySnapshot) {
    // First fetch failed before any data (good or mock) was ever obtained —
    // e.g. unreachable on first launch. Say so plainly rather than rendering
    // an empty accounts table.
    return (
      <div className="win">
        <div className="pane" style={{ justifyContent: "center" }}>
          <div className="empty">
            <h3>Can&apos;t load accounts</h3>
            <p>{error?.message ?? "Unknown error."}</p>
            <button className="btn" onClick={() => void refresh()}>
              Retry
            </button>
          </div>
        </div>
      </div>
    );
  }

  const hasAccounts = displaySnapshot.environments.some((e) => e.accounts.length > 0);

  // Any realm reporting a Claude Code login means the user has one, even
  // though this app is not tracking it yet. `undefined` is undetermined (see
  // paths::claude_login_present) and must not read as "no".
  const loginPresent = displaySnapshot.environments.some((e) => e.hasCredentials === true)
    ? true
    : displaySnapshot.environments.every((e) => e.hasCredentials === false)
      ? false
      : undefined;

  if (!hasAccounts) {
    return (
      <div className="win">
        <FirstRunScreen
          onAction={(action) => (action === "signIn" ? handleInteractiveLogin() : handleAddAccount())}
          pending={pendingInteractiveLogin ? "signIn" : pendingAddAccount ? "addCurrent" : null}
          error={interactiveLoginError ?? addAccountError}
          loginPresent={loginPresent}
        />
      </div>
    );
  }

  const autoOn = settings.settings?.autoSwitchEnabled ?? false;
  const hint = nextHint(displaySnapshot, daemonPhase, settings.settings?.strategy, now);

  // Wraps every screen, so tables and the drawer read the clock and display
  // preferences off context instead of having them prop-drilled through rows.
  // Mounted only here, on the branch that renders real data: the early
  // returns above show no times at all, and both hooks fall back to their
  // defaults without a provider anyway.
  return (
    <ClockFormatProvider value={clockFormat}>
      <DisplayModeProvider value={settings.settings?.displayMode ?? "used"}>
        <div className={`win${!live ? " has-banner" : ""}`}>
          {!live && <SampleDataBanner />}
          <div className="winbody">
            <nav className="nav" aria-label="Main">
              {NAV_ITEMS.map((item) => (
                <button
                  key={item.id}
                  type="button"
                  className={`navlink${screen === item.id ? " is-active" : ""}`}
                  aria-current={screen === item.id ? "page" : undefined}
                  onClick={() => setScreen(item.id)}
                >
                  {item.label}
                  {item.id === "settings" && update.available && (
                    <span
                      className="navlink-update-pill"
                      title={
                        update.status?.kind === "available"
                          ? `Version ${update.status.version} is available — Settings → About to install`
                          : "Update available"
                      }
                    >
                      Update
                    </span>
                  )}
                </button>
              ))}

              <button type="button" className="nav-cmdk" onClick={() => setPaletteOpen(true)}>
                <span>Search</span>
                <span className="kbd">{MOD_KEY} K</span>
              </button>

              <div className="nav-auto">
                <div className="nav-auto-row">
                  <span>Auto-switch</span>
                  <Toggle
                    checked={autoOn}
                    ariaLabel="Auto-switch"
                    disabled={recoveryBlocked || settings.settings === null}
                    onChange={setAutoSwitch}
                  />
                </div>
                <span className="nav-auto-phase">{daemonPhaseLabel(daemonPhase, settings.settings?.strategy)}</span>
                {autoOn && hint && <span className="nav-auto-next">{hint}</span>}
                {autoSwitchError && <span className="nav-auto-err" role="alert">{autoSwitchError}</span>}
              </div>
            </nav>

            <main className="main-content">
              {recoveryBlocked && <MainRecoveryBanner phase={daemonPhase} />}
              {screen === "home" && (
                <HomeScreen
                  snapshot={displaySnapshot}
                  settings={settings.settings}
                  now={now}
                  degraded={error !== null}
                  loginPresent={loginPresent}
                  drawer={drawer}
                  onDrawerChange={setDrawer}
                  showToken={showToken}
                  onShowTokenChange={setShowToken}
                  onSwitch={(n) => handleSwitch(n)}
                  pendingAccount={pendingAccount}
                  switchError={switchError}
                  onAddAccount={handleAddAccount}
                  pendingAddAccount={pendingAddAccount}
                  addAccountError={addAccountError}
                  onAddToken={handleAddToken}
                  pendingAddToken={pendingAddToken}
                  addTokenError={addTokenError}
                  onInteractiveLogin={handleInteractiveLogin}
                  pendingInteractiveLogin={pendingInteractiveLogin}
                  interactiveLoginError={interactiveLoginError}
                  onRelogin={handleRelogin}
                  pendingReloginAccount={pendingReloginAccount}
                  reloginError={reloginError}
                  onSetEnabled={handleSetEnabled}
                  pendingEnableAccount={pendingEnableAccount}
                  enableError={enableError}
                  onRename={handleRename}
                  onRemove={handleRemove}
                  onReorder={handleReorder}
                  onWake={handleWake}
                  pendingWake={pendingWake}
                  wakeError={wakeError}
                  onViewHistory={openHistoryFor}
                  mutationInFlight={mutationInFlight}
                />
              )}
              {screen === "history" && (
                <DashboardScreen
                  snapshot={displaySnapshot}
                  settingsThreshold={settings.settings?.threshold ?? 90}
                  degraded={error !== null}
                  focus={historyFocus}
                />
              )}
              {screen === "settings" && (
                <SettingsScreen
                  runtime={settings}
                  theme={theme.theme}
                  onThemeChange={theme.setTheme}
                  themeError={theme.error}
                  update={update}
                  snapshot={displaySnapshot}
                />
              )}
            </main>
          </div>
        </div>
        {paletteOpen && (
          <Suspense fallback={null}>
            <CommandPalette open={paletteOpen} commands={commands} onClose={() => setPaletteOpen(false)} />
          </Suspense>
        )}
      </DisplayModeProvider>
    </ClockFormatProvider>
  );
}

