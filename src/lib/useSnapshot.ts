/**
 * Owns the app's one snapshot of account/usage state.
 *
 * Fetches once on mount for a fast first paint, then subscribes to
 * `snapshot://updated` — the event the Rust poller emits after every
 * successful tick of its own adaptive-cadence loop (see
 * `src-tauri/src/poller.rs`). This hook does NOT run its own polling timer:
 * the usage API is rate limited per access token, and that poller is the
 * single owner of fetching against it. Two schedulers (this hook's old
 * `setInterval` plus the poller's loop) doubled load against that budget,
 * which is what this hook used to do before it became event-driven — see
 * the incident this fixed: both the main window and the tray popover mount
 * `useSnapshot`, so a per-component timer was really two-plus independent
 * pollers hitting the same per-token budget at once.
 *
 * A failed poll (or one that never arrives) never clears the last good
 * snapshot. The error is surfaced alongside whatever data is still on hand,
 * because a stale labelled number beats a blank screen.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { getSnapshot, hasBackend, onSnapshotUpdated, type Unlisten } from "@/lib/api";
import type { Snapshot } from "@/types";

/**
 * Floor between two manually-triggered refreshes. A user-initiated refresh
 * is legitimate and infrequent by nature, but nothing stops someone from
 * holding a "refresh" button down — this floor keeps that from turning into
 * its own uncoordinated poller against the same per-token budget the Rust
 * poller's cadence protects.
 */
const MIN_REFRESH_INTERVAL_MS = 5_000;

export interface UseSnapshotResult {
  /** Last known snapshot. Null only before the very first fetch resolves. */
  snapshot: Snapshot | null;
  /** False when `snapshot` is `mock.ts` sample data because there is no backend. */
  live: boolean;
  /** True only until the first fetch (success or failure) has resolved. */
  loading: boolean;
  /** Error from the most recent fetch, if it failed. Stale data is kept regardless. */
  error: Error | null;
  /**
   * Re-fetch immediately, e.g. after a mutation or a manual retry. Throttled
   * to at most once per `MIN_REFRESH_INTERVAL_MS` — calls within the floor
   * are dropped silently rather than queued, since the pushed event will
   * bring the next real update anyway.
   */
  refresh: () => Promise<void>;
}

export function useSnapshot(): UseSnapshotResult {
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const [live, setLive] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<Error | null>(null);

  // Guards against a slow response landing after a newer request (or a
  // pushed event) already resolved, or after the component has unmounted.
  const requestId = useRef(0);
  const mounted = useRef(true);
  const lastRefreshAt = useRef(0);

  const load = useCallback(async () => {
    const id = ++requestId.current;
    try {
      const result = await getSnapshot();
      if (!mounted.current || id !== requestId.current) return;
      setSnapshot(result.data);
      setLive(result.live);
      setError(null);
    } catch (err) {
      if (!mounted.current || id !== requestId.current) return;
      // Deliberately does not touch `snapshot` — last good data stays put.
      setError(err instanceof Error ? err : new Error(String(err)));
    } finally {
      if (mounted.current && id === requestId.current) setLoading(false);
    }
  }, []);

  /** User-initiated refresh: throttled, otherwise identical to `load`. */
  const refresh = useCallback(async () => {
    const now = Date.now();
    if (now - lastRefreshAt.current < MIN_REFRESH_INTERVAL_MS) return;
    lastRefreshAt.current = now;
    await load();
  }, [load]);

  useEffect(() => {
    mounted.current = true;
    void load(); // fast first paint; the pushed event keeps it current after this

    if (!hasBackend()) {
      // No poller exists in a plain browser to push updates, and the mock
      // snapshot doesn't change on its own — nothing to subscribe to.
      return () => {
        mounted.current = false;
      };
    }

    let unlisten: Unlisten | undefined;
    let cancelled = false;

    void onSnapshotUpdated((pushed) => {
      if (!mounted.current) return;
      // Supersede any manual `load()` still in flight so its response can't
      // land after (and stomp on) this fresher pushed data.
      requestId.current += 1;
      setSnapshot(pushed);
      setLive(true);
      setError(null);
      setLoading(false);
    }).then((off) => {
      if (cancelled) {
        off();
      } else {
        unlisten = off;
      }
    });

    return () => {
      mounted.current = false;
      cancelled = true;
      unlisten?.();
    };
  }, [load]);

  return { snapshot, live, loading, error, refresh };
}
