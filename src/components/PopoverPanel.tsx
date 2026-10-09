/**
 * Tray popover. Usage data is display-only; every automatic-switch label and
 * action comes from the backend's revisioned `DaemonStatus` contract.
 */

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from "react";

import { Loading } from "@/components/Loading";
import { RefreshButton } from "@/components/RefreshButton";
import UsageMeter from "@/components/UsageMeter";
import PlanBadge from "@/components/PlanBadge";
import AutoSwitchControl from "@/components/popover/AutoSwitchControl";
import { predictTarget, projectLimit, weeklyPaceMark } from "@/lib/coverage";
import { DisplayModeProvider } from "@/lib/displayMode";
import { useBurnSamples } from "@/lib/useBurnSamples";
import { hasBackend, IpcError, switchAccount } from "@/lib/api";
import { formatClock, formatCountdown, useNow } from "@/lib/time";
import { useDaemonStatus } from "@/lib/useDaemonStatus";
import { useSettings } from "@/lib/useSettings";
import { useSnapshot } from "@/lib/useSnapshot";
import { useCliStatus } from "@/lib/useCliStatus";
import { needsMove, useLabel } from "@/lib/sessionCopy";
import { useTheme } from "@/lib/useTheme";
import {
  bindingUtilisation,
  bindingWindow,
  displayName,
  isEnterprise,
  type Usage,
  type UsageWindow,
} from "@/types";

function SampleDataBanner() {
  return (
    <div className="sample-banner" role="status">
      Sample data — these are not your real accounts or usage.
    </div>
  );
}

async function currentPopoverWindow() {
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  return getCurrentWindow();
}

/** Bring up the main window (and put the popover away) for actions that need it. */
async function openMainWindow() {
  if (!hasBackend()) return;
  const [{ Window }, popover] = await Promise.all([import("@tauri-apps/api/window"), currentPopoverWindow()]);
  const main = await Window.getByLabel("main");
  if (main) {
    await main.show();
    await main.unminimize();
    await main.setFocus();
  }
  await popover.hide();
}

function useDismissOnBlurOrEscape() {
  useEffect(() => {
    if (!hasBackend()) return;
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void (async () => {
      const win = await currentPopoverWindow();
      const off = await win.onFocusChanged(({ payload: focused }) => {
        if (!focused) void win.hide();
      });
      if (cancelled) off();
      else unlisten = off;
    })();
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") void currentPopoverWindow().then((win) => win.hide());
    };
    window.addEventListener("keydown", onKeyDown);
    return () => {
      cancelled = true;
      unlisten?.();
      window.removeEventListener("keydown", onKeyDown);
    };
  }, []);
}

/** Never shrink the popover below this, so a bad measurement can't leave an invisible sliver. */
const MIN_POPOVER_HEIGHT = 120;

/**
 * Keep the popover window exactly as tall as its content.
 *
 * The popover lives hidden until a tray click shows it, and a hidden WebView2
 * runs no animation frames. An earlier version deferred `setSize` to
 * `requestAnimationFrame`, so content that changed while hidden never resized
 * the window, and it opened as a 2-pixel sliver (found in a Windows smoke
 * test). So: measure after every render (layout reads work while hidden),
 * apply immediately, re-measure on focus, and clamp to a sane minimum.
 */
function useSizeToContent(ref: { current: HTMLDivElement | null }) {
  const applied = useRef(0);

  const apply = useCallback((height: number) => {
    const next = Math.max(MIN_POPOVER_HEIGHT, Math.ceil(height));
    if (next === applied.current) return;
    applied.current = next;
    void (async () => {
      const [win, { LogicalSize }] = await Promise.all([
        currentPopoverWindow(),
        import("@tauri-apps/api/window"),
      ]);
      await win.setSize(new LogicalSize(364, next));
    })();
  }, []);

  const measure = useCallback(() => {
    const node = ref.current;
    if (node) apply(node.getBoundingClientRect().height);
  }, [ref, apply]);

  // Every commit: data arriving while hidden still resizes the window.
  useLayoutEffect(() => {
    if (hasBackend()) measure();
  });

  useEffect(() => {
    if (!hasBackend()) return;
    const node = ref.current;
    if (!node) return;
    // Late layout changes (fonts, images) with no React render behind them.
    const observer = new ResizeObserver(() => measure());
    observer.observe(node);
    // Shown again: measure fresh in case anything was missed while hidden.
    const onShown = () => {
      applied.current = 0;
      measure();
    };
    window.addEventListener("focus", onShown);
    document.addEventListener("visibilitychange", onShown);
    return () => {
      observer.disconnect();
      window.removeEventListener("focus", onShown);
      document.removeEventListener("visibilitychange", onShown);
    };
  }, [ref, measure]);
}

/**
 * "in 4h 21m" for a window's reset, recomputed locally so it stays honest
 * between the backend's ~5-minute polls. Falls back to the backend's own
 * strings when `resetsAt` is missing or unparseable — stale beats blank, the
 * same order `fresh_reset_strings` uses in src-tauri/src/oauth.rs.
 */
function resetLabel(usage: UsageWindow | undefined, now: number): string {
  return formatCountdown(usage?.resetsAt, now) ?? usage?.countdown ?? usage?.clock ?? "—";
}

/**
 * When the window gating a *switch target* frees up, e.g. "3h 38m".
 *
 * A meter alone says an account is at 88% but not whether that clears in twenty
 * minutes or six days, which is exactly the thing you need before choosing to
 * switch. Read from `bindingWindow` so the time describes the same window the
 * meter beside it measures.
 *
 * Null, not a dash, when nothing is known: these rows are switch buttons, and a
 * column of em dashes reads as a broken readout rather than an absent one.
 */
function bindingReset(usage: Usage | undefined, now: number): string | null {
  const binding = bindingWindow(usage);
  if (!binding) return null;
  return formatCountdown(binding.resetsAt, now) ?? binding.countdown ?? binding.clock ?? null;
}

interface SwitchErrorState {
  accountNumber: number;
  message: string;
}

export default function PopoverPanel() {
  const settings = useSettings();
  const daemon = useDaemonStatus();
  const { snapshot, live, loading, error, refresh } = useSnapshot();
  const rootRef = useRef<HTMLDivElement>(null);
  useTheme(settings);
  useDismissOnBlurOrEscape();
  useSizeToContent(rootRef);

  const [pendingAccount, setPendingAccount] = useState<number | null>(null);
  const [switchError, setSwitchError] = useState<SwitchErrorState | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [now, setNow] = useState(Date.now);
  // Minute-resolution ticker for the reset countdowns, which must keep running
  // in every phase — distinct from the 1s `now` below, armed only while warning.
  const minuteNow = useNow();
  // This window mounts PopoverPanel alone (src/popover.tsx) with no clock-format
  // provider, so take the setting from the settings feed rather than context.
  const clockFormat = settings.settings?.clockFormat ?? "system";

  const phase = daemon.status?.phase;
  const recoveryBlocked = phase?.kind === "recoveryRequired";
  useEffect(() => {
    if (phase?.kind !== "warning") return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [phase]);

  const accounts = useMemo(
    () => snapshot?.environments.flatMap((environment) => environment.accounts) ?? [],
    [snapshot],
  );
  const activeAccount = accounts.find((account) => account.active) ?? null;
  // Local history only — for the "runs out at" line. Never a request to Anthropic.
  const { activeSamples } = useBurnSamples(accounts, snapshot);
  const warningTarget =
    phase?.kind === "warning" ? accounts.find((account) => account.number === phase.to) ?? null : null;

  const cli = useCliStatus();
  // The live swap needs no claude command: the pick lands in `~/.claude`.
  const commandNeeded = cli.missing && settings.settings?.switchRunningSessions !== true;
  const handleSwitch = useCallback(
    (accountNumber: number) => {
      const target = accounts.find((account) => account.number === accountNumber);
      // Moving an account, or installing the claude command it needs, happens
      // in the main window: Move opens a sign-in terminal, and the install
      // prompt explains what it changes.
      if (target && (needsMove(target) || (target.profile && commandNeeded))) {
        void openMainWindow();
        return;
      }
      setPendingAccount(accountNumber);
      setSwitchError(null);
      switchAccount(accountNumber)
        .then(() => refresh())
        .catch((reason: unknown) => {
          const message =
            reason instanceof IpcError && reason.isBusy
              ? "Another process is using your accounts right now. Try again in a moment."
              : reason instanceof IpcError && reason.isReloginRequired
                ? "This account needs a fresh sign-in before it can be activated."
              : reason instanceof Error
                ? reason.message
                : "Couldn't switch accounts.";
          setSwitchError({ accountNumber, message });
        })
        .finally(() => setPendingAccount(null));
    },
    [accounts, commandNeeded, refresh],
  );

  const snooze = useCallback(() => {
    setActionError(null);
    void settings.snooze(3600).catch((reason: unknown) => {
      setActionError(reason instanceof Error ? reason.message : "Couldn't pause auto-switch.");
    });
  }, [settings.snooze]);

  const setAutoSwitch = useCallback(
    (enabled: boolean) => {
      setActionError(null);
      void settings.update({ autoSwitchEnabled: enabled }).catch((reason: unknown) => {
        setActionError(reason instanceof Error ? reason.message : "Couldn't change auto-switch.");
      });
    },
    [settings.update],
  );

  const resume = useCallback(() => {
    setActionError(null);
    void settings.resume().catch((reason: unknown) => {
      setActionError(reason instanceof Error ? reason.message : "Couldn't resume auto-switch.");
    });
  }, [settings.resume]);

  if (loading && !snapshot) {
    return (
      <div className="pop" ref={rootRef}>
        <div style={{ padding: "22px 14px" }}><Loading /></div>
      </div>
    );
  }

  const notConfigured = error instanceof IpcError && error.isNotConfigured;
  if (notConfigured || !snapshot) {
    return (
      <div className="pop" ref={rootRef}>
        <div style={{ padding: "18px 14px", display: "flex", flexDirection: "column", gap: 10 }}>
          <div style={{ fontSize: 13 }}>
            {notConfigured ? "No accounts yet — open the app to add one." : (error?.message ?? "Can't load accounts.")}
          </div>
          {!notConfigured && (
            <button type="button" className="btn" onClick={() => void refresh()}>
              Retry
            </button>
          )}
        </div>
      </div>
    );
  }

  if (!activeAccount) {
    return (
      <div className="pop" ref={rootRef}>
        <div style={{ padding: "18px 14px", fontSize: 13, color: "var(--muted)" }}>No active account.</div>
      </div>
    );
  }

  const activeUtil = bindingUtilisation(activeAccount.usage);
  const others = accounts.filter((account) => account.number !== activeAccount.number);
  const fiveHour = activeAccount.usage?.fiveHour;
  const activeSpend = isEnterprise(activeAccount.usage) ? activeAccount.usage?.spend : undefined;
  const sevenDay = activeAccount.usage?.sevenDay;
  const dimStyle: CSSProperties | undefined = error ? { opacity: 0.55 } : undefined;
  const urgent = phase?.kind === "warning" || phase?.kind === "switching" || phase?.kind === "exhausted";
  const secondsLeft =
    phase?.kind === "warning"
      ? Math.max(0, Math.ceil((Date.parse(phase.deadline) - now) / 1000))
      : null;

  const projection = projectLimit(activeAccount, activeSamples, minuteNow);
  const bestNext = phase?.kind === "warning" ? null : predictTarget(accounts, settings.settings?.strategy ?? "most-headroom", minuteNow);

  return (
    <DisplayModeProvider value={settings.settings?.displayMode ?? "used"}>
    <div className="pop" ref={rootRef}>
      {!live && <SampleDataBanner />}

      {phase?.kind === "warning" && (
        <div className="banner caution" role="status">
          <span title={warningTarget ? displayName(warningTarget) : undefined}>
            Switch planned to {warningTarget ? displayName(warningTarget) : `account ${phase.to}`}
          </span>
          <span className="sp" />
          <span className="num">{secondsLeft === 0 ? "switching now" : `switching in ${secondsLeft}s`}</span>
        </div>
      )}
      {phase?.kind === "switching" && <div className="banner caution">Switching accounts now…</div>}
      {phase?.kind === "exhausted" && <div className="banner danger">All accounts at their limit</div>}
      {phase?.kind === "paused" && (
        <div className="banner caution">
          Paused until {formatClock(phase.until, clockFormat) ?? "an unknown time"}
        </div>
      )}
      {phase?.kind === "cooldown" && (
        <div className="banner caution">
          Cooldown until {formatClock(phase.until, clockFormat) ?? "an unknown time"}
        </div>
      )}
      {phase?.kind === "degraded" && (
        <div className="banner caution">
          {phase.reason === "usageUnknown" ? "Usage is currently unknown" : "The latest usage fetch failed"}
        </div>
      )}
      {phase?.kind === "recoveryRequired" && (
        <div className="banner danger" style={{ display: "block" }}>
          <b>Recovery required</b>
          <div style={{ marginTop: 4 }}>{phase.detail}</div>
        </div>
      )}

      <div className="pop-head">
        <div className="who">
          <span className="mark on" />
          <span className="alias" title={displayName(activeAccount)}>
            {displayName(activeAccount)}
          </span>
          <PlanBadge usage={activeAccount.usage} />
          <span className={`pill ${urgent ? "danger" : "on"}`}>
            {urgent && activeUtil != null ? `${Math.round(activeUtil)}%` : "active"}
          </span>
        </div>
        <div className="pop-win">
          {/* No rate-limit windows exist on an enterprise plan, so the cap it is
              actually limited by takes their place rather than leaving the
              block empty. */}
          {activeSpend && (
            <div className="row">
              <span className="lab">spend</span>
              <div style={dimStyle}><UsageMeter pct={activeSpend.pct} /></div>
              <span className="rst">{resetLabel(activeSpend, minuteNow)}</span>
            </div>
          )}
          {fiveHour && (
            <div className="row">
              <span className="lab">5h</span>
              <div style={dimStyle}><UsageMeter pct={fiveHour.pct} /></div>
              <span className="rst">{resetLabel(fiveHour, minuteNow)}</span>
            </div>
          )}
          {sevenDay && (
            <div className="row">
              <span className="lab">7d</span>
              <div style={dimStyle}><UsageMeter pct={sevenDay.pct} pace={weeklyPaceMark(activeAccount.usage, minuteNow)} /></div>
              <span className="rst">{resetLabel(sevenDay, minuteNow)}</span>
            </div>
          )}
        </div>
        {projection.at !== null && projection.beforeReset && phase?.kind !== "exhausted" && (
          <p className="pop-proj num" style={dimStyle}>
            {projection.at <= minuteNow
              ? "At its limit now."
              : `At this pace it hits the limit ~${formatClock(new Date(projection.at).toISOString(), clockFormat, minuteNow) ?? "soon"}`}
            {projection.resetsAt !== null && projection.at > minuteNow
              ? `, before it resets ${formatClock(new Date(projection.resetsAt).toISOString(), clockFormat, minuteNow) ?? ""}.`
              : ""}
          </p>
        )}
        {phase?.kind === "exhausted" && (
          <p style={{ margin: "12px 0 0", fontSize: 12, color: "var(--muted)" }}>
            {phase.earliestReset
              ? `The earliest known reset is ${formatClock(phase.earliestReset, clockFormat) ?? "an unknown time"}.`
              : "Nothing can be selected automatically until a quota resets."}
          </p>
        )}
      </div>

      <div className="pop-list">
        {others.map((account) => {
          const disabled = account.usageStatus === "disabled";
          const needsRelogin = account.usageStatus === "reloginrequired";
          const hasForeignCredential = account.usageStatus === "foreigncredential";
          const unavailable = disabled || needsRelogin || recoveryBlocked;
          const isNext = phase?.kind === "warning" && phase.to === account.number;
          const isPending = pendingAccount === account.number;
          // Suppressed while unavailable: when an account cannot be switched to,
          // when its quota frees up is not the thing standing in the way.
          const reset = unavailable ? null : bindingReset(account.usage, minuteNow);
          return (
            <button
              key={account.number}
              type="button"
              className={`pop-item${unavailable ? " dim" : ""}${isNext ? " next" : ""}`}
              disabled={unavailable || pendingAccount !== null}
              onClick={() => handleSwitch(account.number)}
            >
              <span className="mark" />
              <span className="alias" title={displayName(account)}>{displayName(account)}</span>
              <PlanBadge usage={account.usage} />
              {disabled && <span className="pill">held out</span>}
              {needsRelogin && <span className="pill danger" title="Re-login required">Re-login</span>}
              {hasForeignCredential && (
                <span className="pill danger" title="Credential mismatch">mismatch</span>
              )}
              {isNext && <span className="pill">next</span>}
              {!isNext && bestNext?.number === account.number && !unavailable && <span className="pill best" title="Auto-switch would pick this account next">best</span>}
              {isPending && <span className="pill">switching…</span>}
              <div className="pop-meter" style={dimStyle}>
                <UsageMeter pct={bindingUtilisation(account.usage)} />
              </div>
              <span className="rst">{reset}</span>
              {!unavailable && (
                <span className="pop-go" aria-hidden="true">
                  {needsMove(account) ? "Move" : useLabel(account, false)}
                </span>
              )}
            </button>
          );
        })}
      </div>

      {(switchError || actionError) && (
        <div style={{ padding: "0 14px 10px", fontSize: 11, color: "var(--danger)" }}>
          {switchError?.message ?? actionError}
        </div>
      )}

      <div className="pop-foot">
        {phase?.kind === "warning" && warningTarget ? (
          <>
            <button
              type="button"
              className="btn"
              disabled={recoveryBlocked || pendingAccount !== null || warningTarget.usageStatus === "reloginrequired"}
              onClick={() => handleSwitch(warningTarget.number)}
            >
              {warningTarget.profile ? "Use now" : "Switch now"}
            </button>
            <button type="button" className="btn ghost" onClick={snooze}>Hold 1h</button>
          </>
        ) : phase?.kind === "paused" ? (
          <button type="button" className="btn" onClick={resume}>Resume</button>
        ) : (
          <>
            <span>{phase?.kind === "disabled" ? "Auto-switch off" : "Auto-switch"}</span>
            <AutoSwitchControl
              phase={phase}
              disabled={recoveryBlocked || settings.settings === null}
              onEnable={() => setAutoSwitch(true)}
              onDisable={() => setAutoSwitch(false)}
              onHold={snooze}
            />
            <span className="sp" />
            <RefreshButton compact />
          </>
        )}
      </div>
    </div>
    </DisplayModeProvider>
  );
}
