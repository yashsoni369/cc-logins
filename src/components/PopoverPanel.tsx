/**
 * Tray popover. Usage data is display-only; every automatic-switch label and
 * action comes from the backend's revisioned `DaemonStatus` contract.
 */

import { useCallback, useEffect, useRef, useState, type CSSProperties } from "react";

import { Loading } from "@/components/Loading";
import { RefreshButton } from "@/components/RefreshButton";
import UsageMeter from "@/components/UsageMeter";
import { hasBackend, IpcError, switchAccount } from "@/lib/api";
import { formatClock, formatCountdown, useNow } from "@/lib/time";
import { useDaemonStatus } from "@/lib/useDaemonStatus";
import { useSettings } from "@/lib/useSettings";
import { useSnapshot } from "@/lib/useSnapshot";
import { useTheme } from "@/lib/useTheme";
import {
  bindingUtilisation,
  bindingWindow,
  displayName,
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

function useSizeToContent(ref: { current: HTMLDivElement | null }) {
  useEffect(() => {
    if (!hasBackend()) return;
    const node = ref.current;
    if (!node) return;
    let frame = 0;
    const apply = async (height: number) => {
      const [win, { LogicalSize }] = await Promise.all([
        currentPopoverWindow(),
        import("@tauri-apps/api/window"),
      ]);
      await win.setSize(new LogicalSize(364, Math.max(1, Math.ceil(height))));
    };
    const observer = new ResizeObserver((entries) => {
      const height = entries[0]?.contentRect.height;
      if (height == null) return;
      cancelAnimationFrame(frame);
      frame = requestAnimationFrame(() => void apply(height));
    });
    observer.observe(node);
    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
    };
  }, [ref]);
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

  const accounts = snapshot?.environments.flatMap((environment) => environment.accounts) ?? [];
  const activeAccount = accounts.find((account) => account.active) ?? null;
  const warningTarget =
    phase?.kind === "warning" ? accounts.find((account) => account.number === phase.to) ?? null : null;

  const handleSwitch = useCallback(
    (accountNumber: number) => {
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
    [refresh],
  );

  const snooze = useCallback(() => {
    setActionError(null);
    void settings.snooze(3600).catch((reason: unknown) => {
      setActionError(reason instanceof Error ? reason.message : "Couldn't pause auto-switch.");
    });
  }, [settings.snooze]);

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
  const sevenDay = activeAccount.usage?.sevenDay;
  const dimStyle: CSSProperties | undefined = error ? { opacity: 0.55 } : undefined;
  const urgent = phase?.kind === "warning" || phase?.kind === "switching" || phase?.kind === "exhausted";
  const secondsLeft =
    phase?.kind === "warning"
      ? Math.max(0, Math.ceil((Date.parse(phase.deadline) - now) / 1000))
      : null;

  return (
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
          <span className={`pill ${urgent ? "danger" : "on"}`}>
            {urgent && activeUtil != null ? `${Math.round(activeUtil)}%` : "active"}
          </span>
        </div>
        <div className="pop-win">
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
              <div style={dimStyle}><UsageMeter pct={sevenDay.pct} /></div>
              <span className="rst">{resetLabel(sevenDay, minuteNow)}</span>
            </div>
          )}
        </div>
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
              {disabled && <span className="pill">held out</span>}
              {needsRelogin && <span className="pill danger" title="Re-login required">Re-login</span>}
              {hasForeignCredential && (
                <span className="pill danger" title="Credential mismatch">mismatch</span>
              )}
              {isNext && <span className="pill">next</span>}
              {isPending && <span className="pill">switching…</span>}
              <div className="pop-meter" style={dimStyle}>
                <UsageMeter pct={bindingUtilisation(account.usage)} />
              </div>
              <span className="rst">{reset}</span>
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
              Switch now
            </button>
            <button type="button" className="btn ghost" onClick={snooze}>Hold 1h</button>
          </>
        ) : phase?.kind === "paused" ? (
          <button type="button" className="btn" onClick={resume}>Resume</button>
        ) : (
          <>
            <span>{phase?.kind === "disabled" ? "Auto-switch off" : "Auto-switch"}</span>
            {phase?.kind !== "disabled" && <span className="pill on">on</span>}
            <span className="sp" />
            <RefreshButton compact />
          </>
        )}
      </div>
    </div>
  );
}
