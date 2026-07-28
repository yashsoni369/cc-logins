import { useCallback, useEffect, useRef, useState } from "react";

import { getDaemonStatus, onDaemonStatusUpdated } from "@/lib/api";
import type { DaemonStatus } from "@/types";

export interface UseDaemonStatusResult {
  status: DaemonStatus | null;
  live: boolean;
  loading: boolean;
  error: unknown;
}

function acceptNewest<T extends { revision: number }>(old: T | null, next: T): T {
  return old === null || next.revision > old.revision ? next : old;
}

export function useDaemonStatus(): UseDaemonStatusResult {
  const [status, setStatus] = useState<DaemonStatus | null>(null);
  const [live, setLive] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<unknown>(null);
  const statusRef = useRef<DaemonStatus | null>(null);

  const accept = useCallback((next: DaemonStatus) => {
    const accepted = acceptNewest(statusRef.current, next);
    statusRef.current = accepted;
    setStatus(accepted);
  }, []);

  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;

    void (async () => {
      try {
        const stop = await onDaemonStatusUpdated((next) => {
          if (!active) return;
          setLive(true);
          accept(next);
        });
        if (!active) {
          stop();
          return;
        }
        unlisten = stop;

        const hydrated = await getDaemonStatus();
        if (!active) return;
        setLive(hydrated.live);
        accept(hydrated.data);
      } catch (reason) {
        if (active) setError(reason);
      } finally {
        if (active) setLoading(false);
      }
    })();

    return () => {
      active = false;
      unlisten?.();
    };
  }, [accept]);

  return { status, live, loading, error };
}
