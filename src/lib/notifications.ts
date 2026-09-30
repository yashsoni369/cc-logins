/**
 * OS notifications for the three events Settings lets the user opt into, and
 * the start-at-login control. Notifications are driven from the main window
 * only — it lives (hidden) for the app's whole lifetime, and a second copy in
 * the popover would announce every event twice.
 *
 * Deciding *whether* to notify is a pure function (`quotaNotices`), tested
 * without a runtime. Sending is best-effort and never throws. Permission is
 * asked for only when the user turns an alert on (`ensureNotificationPermission`),
 * never at launch.
 */

import { useCallback, useEffect, useRef, useState } from "react";

import { hasBackend } from "@/lib/api";
import { formatClock, type ClockFormat } from "@/lib/time";
import { displayName, type Account, type DaemonPhase, type Settings, type Snapshot } from "@/types";

export interface Notice {
  title: string;
  body: string;
}

export interface NoticeState {
  activeNumber: number | null;
  phaseKind: DaemonPhase["kind"] | null;
  relogin: Set<number>;
}

export function noticeState(snapshot: Snapshot | null, phase: DaemonPhase | null | undefined): NoticeState {
  const accounts = snapshot?.environments.flatMap((e) => e.accounts) ?? [];
  return {
    activeNumber: accounts.find((a) => a.active)?.number ?? null,
    phaseKind: phase?.kind ?? null,
    relogin: new Set(accounts.filter((a) => a.usageStatus === "reloginrequired").map((a) => a.number)),
  };
}

/**
 * What to announce, given the state before and after one update.
 *
 * - The active account changing right after the daemon warned or was
 *   switching is an automatic switch. A manual switch is not announced: the
 *   user just made it.
 * - Entering `exhausted` is announced once, not on every poll that stays there.
 * - An account newly needing a fresh sign-in is announced once.
 */
export function quotaNotices(
  prev: NoticeState | null,
  next: NoticeState,
  accounts: Account[],
  phase: DaemonPhase | null | undefined,
  settings: Pick<Settings, "notifyOnSwitch" | "notifyOnExhausted" | "notifyOnExpiry">,
  clockFormat: ClockFormat,
): Notice[] {
  if (!prev) return [];
  const out: Notice[] = [];
  const byNumber = new Map(accounts.map((a) => [a.number, a]));
  const nameOf = (n: number) => {
    const account = byNumber.get(n);
    return account ? displayName(account) : `account ${n}`;
  };

  if (
    settings.notifyOnSwitch &&
    prev.activeNumber !== null &&
    next.activeNumber !== null &&
    next.activeNumber !== prev.activeNumber &&
    (prev.phaseKind === "warning" || prev.phaseKind === "switching")
  ) {
    out.push({
      title: `Switched to ${nameOf(next.activeNumber)}`,
      body: `${nameOf(prev.activeNumber)} was near its limit. Claude Code uses the new account on its next request.`,
    });
  }

  if (settings.notifyOnExhausted && next.phaseKind === "exhausted" && prev.phaseKind !== "exhausted") {
    const reset =
      phase?.kind === "exhausted" && phase.earliestReset ? formatClock(phase.earliestReset, clockFormat) : null;
    out.push({
      title: "All accounts are at their limit",
      body: reset ? `The earliest reset is ${reset}.` : "Nothing can be switched to until a limit resets.",
    });
  }

  if (settings.notifyOnExpiry) {
    for (const number of next.relogin) {
      if (prev.relogin.has(number)) continue;
      out.push({
        title: `${nameOf(number)} needs a fresh sign-in`,
        body: "Its saved login was rejected. Open CC Logins and choose Re-sign in.",
      });
    }
  }

  return out;
}

async function notificationPlugin() {
  return import("@tauri-apps/plugin-notification");
}

/**
 * Asks the OS for permission, if not already granted. Called only from the
 * toggle that turns an alert on. Resolves to whether alerts can be shown.
 *
 * Unsigned macOS builds are expected to be denied (see `updater.notifyUpdate`).
 */
export async function ensureNotificationPermission(): Promise<boolean> {
  if (!hasBackend()) return false;
  try {
    const { isPermissionGranted, requestPermission } = await notificationPlugin();
    if (await isPermissionGranted()) return true;
    return (await requestPermission()) === "granted";
  } catch (error) {
    console.warn("[notifications] permission request failed", error);
    return false;
  }
}

/** Best-effort send. Never prompts, never throws; returns whether it was sent. */
export async function sendOsNotification(notice: Notice): Promise<boolean> {
  if (!hasBackend()) return false;
  try {
    const { isPermissionGranted, sendNotification } = await notificationPlugin();
    if (!(await isPermissionGranted())) return false;
    sendNotification(notice);
    return true;
  } catch (error) {
    console.warn("[notifications] failed to send", error);
    return false;
  }
}

/** Watches snapshots and the daemon phase, and sends whatever `quotaNotices` decides. */
export function useQuotaNotifications(
  snapshot: Snapshot | null,
  phase: DaemonPhase | null | undefined,
  settings: Settings | null,
  clockFormat: ClockFormat,
): void {
  const prev = useRef<NoticeState | null>(null);
  useEffect(() => {
    if (!snapshot || !settings) return;
    const next = noticeState(snapshot, phase);
    const accounts = snapshot.environments.flatMap((e) => e.accounts);
    const notices = quotaNotices(prev.current, next, accounts, phase, settings, clockFormat);
    prev.current = next;
    for (const notice of notices) void sendOsNotification(notice);
  }, [snapshot, phase, settings, clockFormat]);
}

export interface AutostartControl {
  /** What the OS reports; `null` while unknown or unavailable. */
  enabled: boolean | null;
  pending: boolean;
  error: string | null;
  set: (next: boolean) => Promise<void>;
}

/**
 * Start-at-login, with the OS login item as the source of truth — it can be
 * changed outside the app, so the toggle shows `isEnabled()`, not a stored
 * flag. `persist` records the choice in settings after the OS accepted it.
 */
export function useAutostart(persist: (enabled: boolean) => Promise<unknown>): AutostartControl {
  const [enabled, setEnabled] = useState<boolean | null>(null);
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!hasBackend()) return;
    let cancelled = false;
    void import("@tauri-apps/plugin-autostart")
      .then(({ isEnabled }) => isEnabled())
      .then((value) => {
        if (!cancelled) setEnabled(value);
      })
      .catch(() => {
        if (!cancelled) setEnabled(null);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const set = useCallback(
    async (next: boolean) => {
      if (!hasBackend()) return;
      setPending(true);
      setError(null);
      try {
        const { enable, disable, isEnabled } = await import("@tauri-apps/plugin-autostart");
        if (next) await enable();
        else await disable();
        const actual = await isEnabled();
        setEnabled(actual);
        await persist(actual);
      } catch (e) {
        setError(e instanceof Error ? e.message : "Couldn't change the login item.");
      } finally {
        setPending(false);
      }
    },
    [persist],
  );

  return { enabled, pending, error, set };
}
