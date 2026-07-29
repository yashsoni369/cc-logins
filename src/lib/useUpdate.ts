/**
 * Owns the update lifecycle for the main window: when to look, when to say
 * something, and whether installing is safe right now.
 *
 * Mounted once, in the window that has the Settings screen. The popover must
 * not mount it — two schedulers would double every check and could announce the
 * same release twice.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import {
  checkForUpdate,
  dueForAutoCheck,
  installBlockedBy,
  installUpdate,
  notifyUpdate,
  recordAutoCheck,
  recordNotified,
  shouldNotify,
  CHECK_INTERVAL_MS,
  STARTUP_DELAY_MS,
  type DownloadProgress,
  type UpdateCheck,
} from "./updater";
import type { DaemonPhase } from "@/types";

export type InstallState =
  | { kind: "idle" }
  | { kind: "installing"; progress: DownloadProgress }
  | { kind: "failed"; message: string };

export interface UseUpdateResult {
  /** Result of the most recent check, or `null` if none has completed. */
  status: UpdateCheck | null;
  /** True while a check is in flight, from either trigger. */
  checking: boolean;
  install: InstallState;
  /** Why installing is unsafe right now, or `null`. */
  blocked: string | null;
  /** True when a newer version is waiting — drives the nav indicator. */
  available: boolean;
  check: () => Promise<void>;
  startInstall: () => Promise<void>;
}

export function useUpdate(
  autoCheckEnabled: boolean,
  phase: DaemonPhase | null,
): UseUpdateResult {
  const [status, setStatus] = useState<UpdateCheck | null>(null);
  const [checking, setChecking] = useState(false);
  const [install, setInstall] = useState<InstallState>({ kind: "idle" });

  // Guards against two checks overlapping — the 24h timer firing while a manual
  // check is still in flight would otherwise run both.
  const inFlight = useRef(false);
  const mounted = useRef(true);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const run = useCallback(async (automatic: boolean) => {
    if (inFlight.current) return;
    inFlight.current = true;
    if (mounted.current) setChecking(true);

    try {
      const result = await checkForUpdate();
      if (!mounted.current) return;
      setStatus(result);

      if (automatic) recordAutoCheck();

      // Only automatic checks notify. After a manual check the user is already
      // looking at the answer, so a toast would be noise.
      if (automatic && result.kind === "available" && shouldNotify(result.version)) {
        recordNotified(result.version);
        void notifyUpdate(result.version);
      }
    } finally {
      inFlight.current = false;
      if (mounted.current) setChecking(false);
    }
  }, []);

  const check = useCallback(() => run(false), [run]);

  // Automatic checks: once after a startup delay, then on a daily timer. Both
  // are torn down when the setting is turned off.
  useEffect(() => {
    if (!autoCheckEnabled) return;

    const timers: ReturnType<typeof setTimeout>[] = [];

    timers.push(
      setTimeout(() => {
        if (dueForAutoCheck()) void run(true);
      }, STARTUP_DELAY_MS),
    );

    const interval = setInterval(() => {
      if (dueForAutoCheck()) void run(true);
    }, CHECK_INTERVAL_MS);

    return () => {
      timers.forEach(clearTimeout);
      clearInterval(interval);
    };
  }, [autoCheckEnabled, run]);

  const blocked = installBlockedBy(phase);

  const startInstall = useCallback(async () => {
    if (status?.kind !== "available" || blocked) return;

    setInstall({ kind: "installing", progress: { downloaded: 0, total: null } });

    const outcome = await installUpdate(status.update, (progress) => {
      if (mounted.current) setInstall({ kind: "installing", progress });
    });

    // On success the app is already restarting, so only failure needs a state.
    if (!outcome.ok && mounted.current) {
      setInstall({ kind: "failed", message: outcome.message });
    }
  }, [status, blocked]);

  return {
    status,
    checking,
    install,
    blocked,
    available: status?.kind === "available",
    check,
    startInstall,
  };
}
