import { useCallback, useEffect, useRef, useState } from "react";
import { cliStatus, installCli, IpcError } from "@/lib/api";
import type { CliStatus } from "@/types";

export interface UseCliStatusResult {
  /** `undefined` while loading, `null` with no backend. */
  status: CliStatus | null | undefined;
  /**
   * The command is a prerequisite and missing: this build ships the shim and
   * it is not on PATH. Picking an account would not reach plain `claude`.
   */
  missing: boolean;
  /** Installed, but another `claude` comes first on PATH. */
  shadowedBy: string | null;
  installing: boolean;
  error: string | null;
  install: () => Promise<boolean>;
  refresh: () => void;
}

function errorText(error: unknown): string {
  if (error instanceof IpcError) return error.detail ?? error.message;
  return String(error);
}

/**
 * Whether the `claude` command is installed. Picking an account only changes
 * what new sessions use through that command, so the app treats it as a
 * prerequisite once accounts have their own folders.
 */
export function useCliStatus(): UseCliStatusResult {
  const [status, setStatus] = useState<CliStatus | null | undefined>(undefined);
  const [installing, setInstalling] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const mounted = useRef(true);

  const refresh = useCallback(() => {
    cliStatus()
      .then((result) => {
        if (mounted.current) setStatus(result.data);
      })
      .catch(() => {
        if (mounted.current) setStatus(null);
      });
  }, []);

  useEffect(() => {
    mounted.current = true;
    refresh();
    return () => {
      mounted.current = false;
    };
  }, [refresh]);

  const install = useCallback(async () => {
    setInstalling(true);
    setError(null);
    try {
      const next = await installCli();
      if (mounted.current) setStatus(next);
      return true;
    } catch (e) {
      if (mounted.current) setError(errorText(e));
      return false;
    } finally {
      if (mounted.current) setInstalling(false);
    }
  }, []);

  const missing = Boolean(status?.shimAvailable) && status?.health.state === "notInstalled";
  const shadowedBy = status?.health.state === "shadowed" ? status.health.by : null;
  return { status, missing, shadowedBy, installing, error, install, refresh };
}
