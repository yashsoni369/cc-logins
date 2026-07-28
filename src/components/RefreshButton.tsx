import { useEffect, useState } from "react";

import { hasBackend, IpcError, refreshSnapshot } from "../lib/api";

/**
 * Fetch usage now, on demand.
 *
 * The poll cadence is fixed in the backend (see
 * `poller::poll_policy::DEFAULT_INTERVAL_S`), so this is the user's way to get
 * a current reading without waiting for the next tick.
 *
 * It does not hand the new snapshot back to its parent. A successful refresh
 * makes the backend emit `snapshot://updated`, which `App` and the popover
 * already subscribe to, so the numbers arrive by the same route as a poller
 * tick — one path into the UI rather than two that could disagree.
 *
 * The cooldown is the backend's, echoed here rather than invented: pressing
 * Refresh spends a request against the same per-token budget the poller
 * rations, so `retryAfterSeconds` comes back from the process that owns that
 * budget and is authoritative across both windows.
 */
export function RefreshButton({
  disabled = false,
  compact = false,
}: {
  /** Extra reason to disable, on top of "no backend", which is detected here. */
  disabled?: boolean;
  /** Tighter label for the popover, where horizontal space is scarce. */
  compact?: boolean;
}) {
  const [pending, setPending] = useState(false);
  const [deadline, setDeadline] = useState<number | null>(null);
  const [now, setNow] = useState(() => Date.now());
  const [error, setError] = useState<string | null>(null);
  const [note, setNote] = useState<string | null>(null);

  // Drive the countdown, and stop the timer the moment it expires rather than
  // leaving an interval running for the life of the screen.
  useEffect(() => {
    if (deadline === null) return;
    const id = window.setInterval(() => {
      if (Date.now() >= deadline) setDeadline(null);
      else setNow(Date.now());
    }, 500);
    return () => window.clearInterval(id);
  }, [deadline]);

  const remaining = deadline === null ? 0 : Math.max(0, Math.ceil((deadline - now) / 1000));
  const cooling = remaining > 0;
  // Sample data is on screen when there is no backend; offering a Refresh that
  // could only throw would be a control that lies about what it does.
  const unavailable = disabled || !hasBackend();

  async function onClick() {
    setPending(true);
    setError(null);
    setNote(null);
    try {
      const result = await refreshSnapshot();
      setNow(Date.now());
      setDeadline(Date.now() + result.retryAfterSeconds * 1000);
      // Say so explicitly. Silence here would let a refused request look
      // identical to a fetch that returned unchanged numbers.
      if (!result.refreshed) setNote("Checked recently — showing the last reading");
    } catch (err) {
      setError(
        err instanceof IpcError && err.isBusy
          ? "Another process is using your accounts right now. Try again in a moment."
          : err instanceof Error
            ? err.message
            : "Could not refresh.",
      );
    } finally {
      setPending(false);
    }
  }

  const label = pending ? "Refreshing…" : cooling ? `Refresh in ${remaining}s` : "Refresh";

  return (
    <div className={compact ? "refresh compact" : "refresh"}>
      <button
        type="button"
        className="btn ghost"
        onClick={onClick}
        disabled={unavailable || pending || cooling}
        // The visible label already changes to "Refresh in 12s", but that is a
        // countdown a screen reader would re-announce twice a second, so the
        // stable reason lives here instead.
        title={
          unavailable
            ? "Not available while showing sample data"
            : cooling
              ? "Usage can only be read a limited number of times per hour, so refresh is briefly rate-limited"
              : "Fetch usage now"
        }
      >
        {label}
      </button>
      {(error || note) && (
        <span className={error ? "refresh-msg err" : "refresh-msg"} role="status">
          {error ?? note}
        </span>
      )}
    </div>
  );
}
