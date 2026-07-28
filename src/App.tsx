import { useCallback, useEffect, useState, type ReactNode } from "react";
import AccountsScreen from "./components/AccountsScreen";
import HistoryScreen from "./components/HistoryScreen";
import EnvironmentsScreen from "./components/EnvironmentsScreen";
import SettingsScreen from "./components/SettingsScreen";
import FirstRunScreen from "./components/FirstRunScreen";
import { useSnapshot } from "./lib/useSnapshot";
import { useTheme, type Theme } from "./lib/useTheme";
import {
  addCurrentAccount,
  addToken,
  interactiveLogin,
  IpcError,
  setAccountEnabled,
  switchAccount,
} from "./lib/api";
import type { Snapshot } from "./types";

type Screen = "accounts" | "history" | "environments" | "settings";

const NAV_ITEMS: Array<{ id: Screen; label: string }> = [
  { id: "accounts", label: "Accounts" },
  { id: "history", label: "History" },
  { id: "environments", label: "Environments" },
  { id: "settings", label: "Settings" },
];

const NAV_THEME_OPTIONS: Array<{ id: Theme; label: string; icon: ReactNode }> = [
  {
    id: "day",
    label: "Day theme",
    icon: (
      <svg width="13" height="13" viewBox="0 0 16 16" fill="none" aria-hidden="true">
        <circle cx="8" cy="8" r="3.1" stroke="currentColor" strokeWidth="1.4" />
        <path
          d="M8 1.5v1.7M8 12.8v1.7M1.5 8h1.7M12.8 8h1.7M3.6 3.6l1.2 1.2M11.2 11.2l1.2 1.2M3.6 12.4l1.2-1.2M11.2 4.8l1.2-1.2"
          stroke="currentColor"
          strokeWidth="1.3"
          strokeLinecap="round"
        />
      </svg>
    ),
  },
  {
    id: "night",
    label: "Night theme",
    icon: (
      <svg width="13" height="13" viewBox="0 0 16 16" fill="none" aria-hidden="true">
        <path
          d="M13.2 9.4A5.6 5.6 0 116.6 2.8a4.4 4.4 0 006.6 6.6z"
          stroke="currentColor"
          strokeWidth="1.4"
          strokeLinejoin="round"
        />
      </svg>
    ),
  },
  {
    id: "system",
    label: "Match system theme",
    icon: (
      <svg width="13" height="13" viewBox="0 0 16 16" fill="none" aria-hidden="true">
        <rect x="2" y="3" width="12" height="8" rx="1.2" stroke="currentColor" strokeWidth="1.4" />
        <path d="M6 13.5h4M8 11v2.3" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
      </svg>
    ),
  },
];

/**
 * Compact Day/Night/System control anchored to the bottom of the sidebar —
 * a quiet utility, not a headline, so it's a three-icon row rather than the
 * full-width `Segmented` the Settings screen uses (which would dominate a
 * narrow sidebar). Real `<button>`s, so Enter/Space activation and the
 * global `:focus-visible` ring come for free without extra key handling.
 *
 * Takes the same `useTheme()` instance `App` already mounts and passes to
 * `SettingsScreen`, rather than mounting its own — two instances would each
 * think they own `document.documentElement` and could disagree.
 */
function NavThemeControl({ theme, onChange }: { theme: Theme; onChange: (t: Theme) => void }) {
  return (
    <div className="nav-theme" role="radiogroup" aria-label="Theme">
      {NAV_THEME_OPTIONS.map((opt) => (
        <button
          key={opt.id}
          type="button"
          role="radio"
          aria-checked={theme === opt.id}
          aria-label={opt.label}
          title={opt.label}
          className={`nav-theme-btn${theme === opt.id ? " on" : ""}`}
          onClick={() => onChange(opt.id)}
        >
          {opt.icon}
        </button>
      ))}
    </div>
  );
}

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
  "Another process (very likely the cswap CLI) is using your accounts right now. Try again in a moment.";

/** Message for a failed "Add account" — worded specifically for the one failure users will actually hit. */
function describeAddAccountError(err: unknown): string {
  if (err instanceof IpcError) {
    if (err.isBusy) return BUSY_MESSAGE;
    const detail = err.detail?.toLowerCase() ?? "";
    if (detail.includes("already")) {
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
function describeInteractiveLoginError(err: unknown): string | null {
  if (err instanceof IpcError) {
    if (err.isBusy) return BUSY_MESSAGE;
    const detail = err.detail?.toLowerCase() ?? "";
    if (detail.includes("cancel")) {
      return null;
    }
    if (detail.includes("time")) {
      return "Timed out waiting for sign-in. Nothing was added — try again when you're ready.";
    }
    if (detail.includes("not installed") || detail.includes("not found") || detail.includes("path")) {
      return "Claude Code isn't installed, or the `claude` command isn't on PATH. Install it, then try again.";
    }
    if (detail.includes("terminal")) {
      return 'Couldn\'t open a terminal on this system. Use "Add token" below instead.';
    }
    if (detail.includes("already")) {
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
    const detail = err.detail?.toLowerCase() ?? "";
    if (!enabled && (detail.includes("active") || detail.includes("current"))) {
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

export default function App() {
  const [screen, setScreen] = useState<Screen>("accounts");
  const { snapshot, live, loading, error, refresh } = useSnapshot();
  // Applies the persisted theme to this window's <html> and keeps it live
  // against OS changes. SettingsScreen gets the same instance as props
  // rather than mounting its own, so the segmented control there and the
  // theme actually applied to this document never disagree.
  const theme = useTheme();

  const [pendingAccount, setPendingAccount] = useState<number | null>(null);
  const [switchError, setSwitchError] = useState<SwitchError | null>(null);

  const [pendingAddAccount, setPendingAddAccount] = useState(false);
  const [addAccountError, setAddAccountError] = useState<string | null>(null);

  const [pendingInteractiveLogin, setPendingInteractiveLogin] = useState(false);
  const [interactiveLoginError, setInteractiveLoginError] = useState<string | null>(null);

  const [pendingAddToken, setPendingAddToken] = useState(false);
  const [addTokenError, setAddTokenError] = useState<string | null>(null);

  const [pendingEnableAccount, setPendingEnableAccount] = useState<number | null>(null);
  const [enableError, setEnableError] = useState<EnableError | null>(null);

  // Every mutation below returns the post-change Snapshot, which we show
  // immediately rather than refetching or guessing. It stands in for the
  // poller's own `snapshot` only until that poller's next tick lands (see the
  // effect below), at which point the real, freshly-polled data takes back
  // over.
  const [snapshotOverride, setSnapshotOverride] = useState<Snapshot | null>(null);
  useEffect(() => {
    setSnapshotOverride(null);
  }, [snapshot]);

  // True while any mutating call is in flight. All mutating buttons across
  // the Accounts screen disable together while this is true: they all touch
  // the same single-writer credential store the backend guards with its
  // "busy" lock, so nothing is gained by letting two race to hit it at once.
  const mutationInFlight =
    pendingAccount !== null ||
    pendingAddAccount ||
    pendingAddToken ||
    pendingInteractiveLogin ||
    pendingEnableAccount !== null;

  // The ONLY call site for the mutating `switchAccount`. It is wired
  // exclusively to an onClick handler in AccountsScreen — never invoked from
  // an effect, a timer, or on mount.
  const handleSwitch = useCallback(
    (accountNumber: number) => {
      setPendingAccount(accountNumber);
      setSwitchError(null);
      switchAccount(accountNumber)
        .then(() => refresh())
        .catch((err: unknown) => {
          const message =
            err instanceof IpcError && err.isBusy
              ? BUSY_MESSAGE
              : err instanceof Error
                ? err.message
                : "Couldn't switch accounts.";
          setSwitchError({ accountNumber, message });
        })
        .finally(() => setPendingAccount(null));
    },
    [refresh],
  );

  // The ONLY call site for `addCurrentAccount`. Wired exclusively to the Add
  // account button's onClick in AccountsScreen.
  const handleAddAccount = useCallback(() => {
    setPendingAddAccount(true);
    setAddAccountError(null);
    addCurrentAccount()
      .then((result) => setSnapshotOverride(result))
      .catch((err: unknown) => setAddAccountError(describeAddAccountError(err)))
      .finally(() => setPendingAddAccount(false));
  }, []);

  // The ONLY call site for `interactiveLogin`. Wired exclusively to
  // SignInFlow's own button onClick — never invoked from an effect, a timer,
  // or on mount, same as every other mutation here.
  const handleInteractiveLogin = useCallback(() => {
    setPendingInteractiveLogin(true);
    setInteractiveLoginError(null);
    interactiveLogin()
      .then((result) => setSnapshotOverride(result))
      .catch((err: unknown) => {
        // `null` means the user cancelled by closing the terminal — quiet,
        // not an error, so nothing is set and SignInFlow returns to rest.
        const message = describeInteractiveLoginError(err);
        if (message !== null) setInteractiveLoginError(message);
      })
      .finally(() => setPendingInteractiveLogin(false));
  }, []);

  // The ONLY call site for `addToken`. Wired exclusively to AddTokenDialog's
  // onSubmit, itself only reachable from that form's submit button.
  const handleAddToken = useCallback(async (token: string, email?: string, alias?: string) => {
    setPendingAddToken(true);
    setAddTokenError(null);
    try {
      const result = await addToken(token, email, alias);
      setSnapshotOverride(result);
    } catch (err) {
      setAddTokenError(describeAddTokenError(err));
      throw err; // lets the dialog know not to close itself
    } finally {
      setPendingAddToken(false);
    }
  }, []);

  // The ONLY call site for `setAccountEnabled`. Wired exclusively to a row's
  // Enable/Disable button onClick in AccountsScreen.
  const handleSetEnabled = useCallback((accountNumber: number, enabled: boolean) => {
    setPendingEnableAccount(accountNumber);
    setEnableError(null);
    setAccountEnabled(accountNumber, enabled)
      .then((result) => setSnapshotOverride(result))
      .catch((err: unknown) => setEnableError({ accountNumber, message: describeEnableError(err, enabled) }))
      .finally(() => setPendingEnableAccount(null));
  }, []);

  // An unconfigured machine is a normal state, not an error — it routes to
  // the same first-run screen as "zero accounts found" below.
  const notConfigured = error instanceof IpcError && error.isNotConfigured;

  if (notConfigured) {
    return (
      <div className="win">
        <FirstRunScreen
          onAction={(action) =>
            action === "signIn" ? handleInteractiveLogin() : handleAddAccount()
          }
          pending={
            pendingInteractiveLogin ? "signIn" : pendingAddAccount ? "addCurrent" : null
          }
          error={interactiveLoginError ?? addAccountError}
        />
      </div>
    );
  }

  if (loading && !snapshot) {
    return (
      <div className="win">
        <div className="pane" style={{ justifyContent: "center", alignItems: "center" }}>
          <span className="sub">Loading…</span>
        </div>
      </div>
    );
  }

  if (!snapshot) {
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

  // Narrowed to non-null only past the guards above, so it's safe to derive here:
  // the returned Snapshot from a mutation, shown until the poller's next tick lands.
  const displaySnapshot: Snapshot = snapshotOverride ?? snapshot;

  const hasAccounts = displaySnapshot.environments.some((e) => e.accounts.length > 0);

  if (!hasAccounts) {
    return (
      <div className="win">
        <FirstRunScreen
          onAction={(action) =>
            action === "signIn" ? handleInteractiveLogin() : handleAddAccount()
          }
          pending={
            pendingInteractiveLogin ? "signIn" : pendingAddAccount ? "addCurrent" : null
          }
          error={interactiveLoginError ?? addAccountError}
        />
      </div>
    );
  }

  return (
    <div className={`win${!live ? " has-banner" : ""}`}>
      {!live && <SampleDataBanner />}
      <div className="winbody">
        <div className="nav">
          {NAV_ITEMS.map((item) => (
            <button
              key={item.id}
              type="button"
              className={`navlink${screen === item.id ? " is-active" : ""}`}
              onClick={() => setScreen(item.id)}
            >
              {item.label}
            </button>
          ))}
          <div className="grp">Auto-switch</div>
          <span>Running · best</span>

          <NavThemeControl theme={theme.theme} onChange={theme.setTheme} />
        </div>

        {screen === "accounts" && (
          <AccountsScreen
            snapshot={displaySnapshot}
            onSwitch={handleSwitch}
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
            onSetEnabled={handleSetEnabled}
            pendingEnableAccount={pendingEnableAccount}
            enableError={enableError}
            mutationInFlight={mutationInFlight}
            degraded={error !== null}
          />
        )}
        {screen === "history" && <HistoryScreen />}
        {screen === "environments" && <EnvironmentsScreen environments={displaySnapshot.environments} />}
        {screen === "settings" && (
          <SettingsScreen
            theme={theme.theme}
            onThemeChange={theme.setTheme}
            themeError={theme.error}
          />
        )}
      </div>
    </div>
  );
}
