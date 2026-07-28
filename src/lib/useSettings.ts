import { useCallback, useEffect, useRef, useState } from "react";

import {
  getSettingsSnapshot,
  IpcError,
  onSettingsUpdated,
  resumeAutoSwitch,
  snoozeAutoSwitch,
  updateSettings,
} from "@/lib/api";
import type { Settings, SettingsPatch, SettingsSnapshot } from "@/types";

export interface UseSettingsResult {
  snapshot: SettingsSnapshot | null;
  settings: Settings | null;
  live: boolean;
  loading: boolean;
  error: unknown;
  update: (patch: SettingsPatch) => Promise<SettingsSnapshot>;
  snooze: (durationSeconds: number) => Promise<SettingsSnapshot>;
  resume: () => Promise<SettingsSnapshot>;
}

function acceptNewest<T extends { revision: number }>(old: T | null, next: T): T {
  return old === null || next.revision > old.revision ? next : old;
}

export function useSettings(): UseSettingsResult {
  const [snapshot, setSnapshot] = useState<SettingsSnapshot | null>(null);
  const [live, setLive] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<unknown>(null);
  const snapshotRef = useRef<SettingsSnapshot | null>(null);
  const writeQueue = useRef<Promise<void>>(Promise.resolve());

  const accept = useCallback((next: SettingsSnapshot) => {
    const accepted = acceptNewest(snapshotRef.current, next);
    snapshotRef.current = accepted;
    setSnapshot(accepted);
  }, []);

  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;

    void (async () => {
      try {
        const stop = await onSettingsUpdated((next) => {
          if (!active) return;
          setLive(true);
          accept(next);
        });
        if (!active) {
          stop();
          return;
        }
        unlisten = stop;

        const hydrated = await getSettingsSnapshot();
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

  const enqueue = useCallback(
    (operation: (revision: number) => Promise<SettingsSnapshot>): Promise<SettingsSnapshot> => {
      const result = writeQueue.current.then(async () => {
        const current = snapshotRef.current;
        if (!current) throw new Error("Settings have not hydrated yet.");
        setError(null);
        try {
          const saved = await operation(current.revision);
          accept(saved);
          return saved;
        } catch (reason) {
          if (reason instanceof IpcError && reason.kind === "settingsConflict") {
            const confirmed = await getSettingsSnapshot();
            setLive(confirmed.live);
            accept(confirmed.data);
          }
          setError(reason);
          throw reason;
        }
      });
      writeQueue.current = result.then(
        () => undefined,
        () => undefined,
      );
      return result;
    },
    [accept],
  );

  const update = useCallback(
    (patch: SettingsPatch) => enqueue((revision) => updateSettings(revision, patch)),
    [enqueue],
  );
  const snooze = useCallback(
    (durationSeconds: number) => enqueue(() => snoozeAutoSwitch(durationSeconds)),
    [enqueue],
  );
  const resume = useCallback(() => enqueue(() => resumeAutoSwitch()), [enqueue]);

  return {
    snapshot,
    settings: snapshot?.settings ?? null,
    live,
    loading,
    error,
    update,
    snooze,
    resume,
  };
}
