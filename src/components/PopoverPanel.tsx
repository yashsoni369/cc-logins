/**
 * The tray popover — screens 02 and 03 of the wireframe, and the surface
 * users see 90% of the time. Anchored under the tray icon, it answers one
 * question ("where do I stand, and do I need to act?") and lets a click
 * switch accounts without ever opening the full dashboard.
 *
 * Data comes from the same `useSnapshot` poller the main window uses — this
 * file does not start a second one. `previewTarget` is read alongside every
 * poll purely to power the "next" hint and the exhausted-state check; it is
 * never used to trigger a switch on its own. The only path that calls the
 * mutating `switchAccount` is an explicit click (a list row or "Switch now").
 */

import { useCallback, useEffect, useRef, useState, type CSSProperties } from "react";
import type { Account } from "@/types";
import { ageLabel, bindingUtilisation, displayName, quotaState } from "@/types";
import { hasBackend, IpcError, previewTarget, switchAccount } from "@/lib/api";
import { useSnapshot } from "@/lib/useSnapshot";
import { useTheme } from "@/lib/useTheme";
import UsageMeter from "@/components/UsageMeter";

/** Matches the Settings screen's default auto-switch threshold and grace period. */
const GRACE_PERIOD_SECONDS = 60;
/** How long "Hold 1h" suppresses the limit banner for, client-side only. */
const HOLD_DURATION_MS = 60 * 60 * 1000;

/**
 * Persistent, non-dismissible notice that this popover is showing `mock.ts`
 * sample data because no Tauri backend is present. Same approach as
 * `App.tsx`'s `SampleDataBanner` — not imported from there because it isn't
 * exported, but deliberately identical: no close affordance, no colour
 * (this isn't a quota state), unmissable via full width and inverted
 * contrast instead.
 */
function SampleDataBanner() {
  return (
    <div className="sample-banner" role="status">
      Sample data — these are not your real accounts or usage.
    </div>
  );
}

/** Lazily imports the window API so a plain-browser session never loads it. */
async function currentPopoverWindow() {
  const { getCurrentWindow } = await import("@tauri-apps/api/window");
  return getCurrentWindow();
}

/**
 * Hides the popover the way a real menu behaves: losing focus, or Escape.
 * Window show/hide is not credential-mutating, so unlike `switchAccount` it
 * is fine to trigger from an effect.
 */
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
      if (cancelled) {
        off();
      } else {
        unlisten = off;
      }
    })();

    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        void currentPopoverWindow().then((win) => win.hide());
      }
    };
    window.addEventListener("keydown", onKeyDown);

    return () => {
      cancelled = true;
      unlisten?.();
      window.removeEventListener("keydown", onKeyDown);
    };
  }, []);
}

/**
 * Grows/shrinks the popover window to match its own content height. The
 * window is `resizable: false` (no user drag-resize) but that does not
 * block a programmatic `setSize`, which is how "height sized to content"
 * from a fixed-width window is achieved without hardcoding a height.
 */
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
      await win.setSize(new LogicalSize(340, Math.max(1, Math.ceil(height))));
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

/** Per-account outcome of the most recent switch attempt. */
interface SwitchErrorState {
  accountNumber: number;
  message: string;
}

export default function PopoverPanel() {
  const { snapshot, live, loading, error, refresh } = useSnapshot();
  const rootRef = useRef<HTMLDivElement>(null);

  // This popover is its own browsing context with its own <html> — the
  // dashboard window's useTheme() call in App.tsx does not reach it, so it
  // applies (and keeps live) the persisted theme itself. There is no theme
  // control here; the popover only ever displays what Settings decided.
  useTheme();

  useDismissOnBlurOrEscape();
  useSizeToContent(rootRef);

  const [pendingAccount, setPendingAccount] = useState<number | null>(null);
  const [switchError, setSwitchError] = useState<SwitchErrorState | null>(null);
  // undefined = not fetched yet; null = no viable target right now.
  const [target, setTarget] = useState<Account | null | undefined>(undefined);
  const [heldUntil, setHeldUntil] = useState<number | null>(null);
  const [notifyAtReset, setNotifyAtReset] = useState(false);
  const [now, setNow] = useState(() => Date.now());
  const armedSinceRef = useRef<number | null>(null);

  const accounts = snapshot?.environments.flatMap((e) => e.accounts) ?? [];
  const activeAccount = accounts.find((a) => a.active) ?? null;
  const activeUtil = bindingUtilisation(activeAccount?.usage);
  const activeState = quotaState(activeUtil);

  // Read-only, refreshed alongside every snapshot poll. Powers the "next"
  // hint and the exhausted check only — never reachable from a switch path.
  useEffect(() => {
    let cancelled = false;
    previewTarget()
      .then((t) => {
        if (!cancelled) setTarget(t);
      })
      .catch(() => {
        if (!cancelled) setTarget(null);
      });
    return () => {
      cancelled = true;
    };
  }, [snapshot]);

  const isHeld = heldUntil != null && now < heldUntil;
  const isDanger = activeState === "danger" && activeUtil != null;
  const hasTarget = target != null;
  const armed = isDanger && hasTarget && !isHeld;
  const exhausted = isDanger && target === null;

  // A healthy popover has nothing counting down — the tick only runs while
  // there's something time-sensitive on screen (an armed countdown, or a
  // hold waiting to expire).
  useEffect(() => {
    if (!armed && !isHeld) return;
    const id = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(id);
  }, [armed, isHeld]);

  useEffect(() => {
    if (armed) {
      armedSinceRef.current ??= Date.now();
    } else {
      armedSinceRef.current = null;
    }
  }, [armed]);

  const secondsLeft = armed
    ? Math.max(0, GRACE_PERIOD_SECONDS - Math.floor((now - (armedSinceRef.current ?? now)) / 1000))
    : null;

  const earliestReset = accounts.reduce<{ at: number; clock: string; alias: string } | null>((best, a) => {
    const resetsAt = a.usage?.fiveHour?.resetsAt;
    const clock = a.usage?.fiveHour?.clock;
    if (!resetsAt || !clock) return best;
    const at = Date.parse(resetsAt);
    if (Number.isNaN(at)) return best;
    if (!best || at < best.at) return { at, clock, alias: displayName(a) };
    return best;
  }, null);

  // The ONLY call site for the mutating `switchAccount` in this file, wired
  // exclusively to onClick handlers below — never an effect, timer, or the
  // countdown reaching zero.
  const handleSwitch = useCallback(
    (accountNumber: number) => {
      setPendingAccount(accountNumber);
      setSwitchError(null);
      switchAccount(accountNumber)
        .then(() => refresh())
        .catch((err: unknown) => {
          const message =
            err instanceof IpcError && err.isBusy
              ? "Another process (very likely the cswap CLI) is using your accounts right now. Try again in a moment."
              : err instanceof Error
                ? err.message
                : "Couldn't switch accounts.";
          setSwitchError({ accountNumber, message });
        })
        .finally(() => setPendingAccount(null));
    },
    [refresh],
  );

  if (loading && !snapshot) {
    return (
      <div className="pop" ref={rootRef}>
        <div style={{ padding: "22px 14px", fontSize: 13, color: "var(--muted)" }}>Loading…</div>
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

  const others = accounts.filter((a) => a.number !== activeAccount.number);
  const fiveHour = activeAccount.usage?.fiveHour;
  const sevenDay = activeAccount.usage?.sevenDay;
  const activeAge = ageLabel(activeAccount.usageAgeSeconds);
  // Last-good values render regardless, per the staleness rule — they just
  // dim rather than look freshly confirmed when the latest poll failed.
  const dimStyle: CSSProperties | undefined = error ? { opacity: 0.55 } : undefined;

  const showBanner = armed || exhausted;
  const headerPillClass = showBanner ? "danger" : "on";
  // Never print a number we do not have: an unreadable active account shows
  // "no reading" rather than a reassuring 0%.
  const headerPillText = showBanner
    ? activeUtil == null
      ? "no reading"
      : `${Math.round(activeUtil)}%`
    : "active";

  const bindingLabel =
    activeUtil != null && fiveHour?.pct === activeUtil ? "5-hour" : activeUtil != null && sevenDay?.pct === activeUtil ? "7-day" : "quota";

  return (
    <div className="pop" ref={rootRef}>
      {!live && <SampleDataBanner />}

      {exhausted && (
        <div className="banner danger" role="status">
          <span>All accounts at their limit</span>
        </div>
      )}
      {armed && (
        <div className="banner caution" role="status">
          <span>{bindingLabel} limit in reach</span>
          <span className="sp"></span>
          <span className="num">{secondsLeft === 0 ? "switching any moment" : `switching in ${secondsLeft}s`}</span>
        </div>
      )}

      <div className="pop-head">
        <div className="who">
          <span className="mark on"></span>
          <span className="alias">{displayName(activeAccount)}</span>
          <span className={`pill ${headerPillClass}`}>{headerPillText}</span>
          {activeAge && <span className="pill">{activeAge}</span>}
        </div>
        <div className="pop-win">
          {fiveHour && (
            <div className="row">
              <span className="lab">5h</span>
              <div style={dimStyle}>
                <UsageMeter pct={fiveHour.pct} />
              </div>
              <span className="rst">{fiveHour.clock ?? "—"}</span>
            </div>
          )}
          {sevenDay && (
            <div className="row">
              <span className="lab">7d</span>
              <div style={dimStyle}>
                <UsageMeter pct={sevenDay.pct} />
              </div>
              <span className="rst">{sevenDay.clock ?? "—"}</span>
            </div>
          )}
        </div>
        {exhausted && (
          <p style={{ margin: "12px 0 0", fontSize: 12, color: "var(--muted)" }}>
            {earliestReset ? (
              <>
                Earliest reset is <span className="num">{earliestReset.clock}</span> on{" "}
                <b style={{ fontWeight: 550 }}>{earliestReset.alias}</b>. Nothing to switch to until then.
              </>
            ) : (
              "Nothing to switch to until a quota resets."
            )}
          </p>
        )}
      </div>

      <div className="pop-list">
        {others.map((a) => {
          const isHeldOut = a.usageStatus === "disabled";
          // Nullable, not coerced: an unreadable account must render as
          // "no reading", never as a confident 0% that looks like plenty
          // of headroom. UsageMeter handles null explicitly.
          const worst = bindingUtilisation(a.usage);
          const isNext = armed && target?.number === a.number;
          const age = ageLabel(a.usageAgeSeconds);
          const isPending = pendingAccount === a.number;

          return (
            <button
              key={a.number}
              type="button"
              className={`pop-item${isHeldOut ? " dim" : ""}${isNext ? " next" : ""}`}
              disabled={isHeldOut || pendingAccount !== null}
              onClick={() => handleSwitch(a.number)}
            >
              <span className="mark"></span>
              <span className="alias">{displayName(a)}</span>
              {isHeldOut && <span className="pill">held out</span>}
              {isNext && <span className="pill">next</span>}
              {isPending && <span className="pill">switching…</span>}
              {!isHeldOut && !isPending && age && <span className="pill">{age}</span>}
              <div style={dimStyle}>
                <UsageMeter pct={worst} />
              </div>
            </button>
          );
        })}
      </div>

      {switchError && (
        <div style={{ padding: "0 14px 10px", fontSize: 11, color: "var(--danger)" }}>{switchError.message}</div>
      )}

      <div className="pop-foot">
        {exhausted ? (
          <>
            <span>Notify me at reset</span>
            <span className="sp"></span>
            <span
              role="switch"
              aria-checked={notifyAtReset}
              tabIndex={0}
              style={{ display: "inline-flex", cursor: "pointer" }}
              onClick={() => setNotifyAtReset((v) => !v)}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") {
                  e.preventDefault();
                  setNotifyAtReset((v) => !v);
                }
              }}
            >
              <span className={`sw${notifyAtReset ? " on" : ""}`}></span>
            </span>
          </>
        ) : armed && target ? (
          <>
            <button type="button" className="btn" disabled={pendingAccount !== null} onClick={() => handleSwitch(target.number)}>
              Switch now
            </button>
            <button type="button" className="btn ghost" onClick={() => setHeldUntil(Date.now() + HOLD_DURATION_MS)}>
              Hold 1h
            </button>
          </>
        ) : (
          <>
            <span>Auto-switch</span>
            <span className="pill on">on</span>
            <span className="sp"></span>
            <span className="kbd">Ctrl+Shift+A</span>
          </>
        )}
      </div>
    </div>
  );
}
