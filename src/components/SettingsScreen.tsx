import { useCallback, useEffect, useRef, useState } from "react";
import { getSettings, IpcError, setSettings } from "../lib/api";
import type { Theme } from "../lib/useTheme";
import type { Settings } from "../types";

/** A `.toggle`/`.sw` switch, made operable: click or Enter/Space to flip it. */
function Toggle({ checked, onChange, label }: { checked: boolean; onChange: (v: boolean) => void; label: string }) {
  return (
    <span
      className="toggle"
      role="switch"
      aria-checked={checked}
      tabIndex={0}
      onClick={() => onChange(!checked)}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          onChange(!checked);
        }
      }}
    >
      <span className={`sw${checked ? " on" : ""}`}></span> {label}
    </span>
  );
}

interface SegOption<T extends string> {
  id: T;
  label: string;
}

/** A `.seg` segmented control, made operable the same way. */
function Segmented<T extends string>({
  options,
  value,
  onChange,
  ariaLabel,
}: {
  options: Array<SegOption<T>>;
  value: T;
  onChange: (v: T) => void;
  ariaLabel: string;
}) {
  return (
    <div className="seg" role="radiogroup" aria-label={ariaLabel}>
      {options.map((opt) => (
        <span
          key={opt.id}
          role="radio"
          aria-checked={opt.id === value}
          tabIndex={0}
          className={opt.id === value ? "on" : undefined}
          onClick={() => onChange(opt.id)}
          onKeyDown={(e) => {
            if (e.key === "Enter" || e.key === " ") {
              e.preventDefault();
              onChange(opt.id);
            }
          }}
        >
          {opt.label}
        </span>
      ))}
    </div>
  );
}

/** Discretised view of `graceSeconds` — the real field is continuous, this control picks among three common values. */
type GracePeriod = "off" | "60s" | "5m";

const GRACE_SECONDS: Record<GracePeriod, number> = { off: 0, "60s": 60, "5m": 300 };

/** Nearest `GracePeriod` bucket for an arbitrary `graceSeconds` value, so a value set outside this UI (or by a future screen) still displays sensibly. */
function graceBucket(seconds: number): GracePeriod {
  const options: GracePeriod[] = ["off", "60s", "5m"];
  return options.reduce((best, id) =>
    Math.abs(GRACE_SECONDS[id] - seconds) < Math.abs(GRACE_SECONDS[best] - seconds) ? id : best,
  );
}

const THRESHOLD_MIN = 50;
const THRESHOLD_MAX = 99; // matches the backend's clamp range exactly

/** Debounce, in ms, before a slider drag's final value is sent to the backend. */
const SLIDER_COMMIT_MS = 400;

const STRATEGY_OPTIONS: Array<SegOption<Settings["strategy"]>> = [
  { id: "most-headroom", label: "Most headroom" },
  { id: "next-available", label: "Next available" },
  { id: "consume-first", label: "Consume first" },
];

const GRACE_OPTIONS: Array<SegOption<GracePeriod>> = [
  { id: "off", label: "Off" },
  { id: "60s", label: "60s" },
  { id: "5m", label: "5m" },
];

const THEME_OPTIONS: Array<SegOption<Theme>> = [
  { id: "day", label: "Day" },
  { id: "night", label: "Night" },
  { id: "system", label: "System" },
];

interface SettingsScreenProps {
  /** Current theme preference — owned by `useTheme()` in `App.tsx`, passed down so this control and the theme actually applied to the window never disagree. */
  theme: Theme;
  onThemeChange: (theme: Theme) => void;
  /** Set when the most recent theme save failed. The visual change happens regardless — see `useTheme.ts`. */
  themeError: string | null;
}

/**
 * Everything auto-switch does, visible and adjustable — a background process
 * that moves credentials has to be legible. The credential-storage control
 * at the bottom is the one open product decision, surfaced as a real control
 * rather than hidden in a config file.
 *
 * Loads real settings on mount and persists every change via `set_settings`.
 * The threshold slider is debounced so a drag sends one write, not one per
 * pixel; every other control commits immediately since a discrete click is
 * already a single deliberate change. Every commit replaces local state with
 * the backend's *returned* (clamped) settings rather than the value sent —
 * echoing the request would show a number that was not actually saved.
 */
export default function SettingsScreen({ theme, onThemeChange, themeError }: SettingsScreenProps) {
  const [settings, setSettingsState] = useState<Settings | null>(null);
  const [live, setLive] = useState(false);
  const [loading, setLoading] = useState(true);
  const [saveError, setSaveError] = useState<string | null>(null);

  // Local draft for the slider only, so dragging feels instant even though
  // the backend write is debounced. Cleared once the backend echoes back.
  const [draftThreshold, setDraftThreshold] = useState<number | null>(null);

  const commitTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const mounted = useRef(true);
  // Always mirrors the currently *displayed* settings (confirmed plus any
  // optimistic field not yet round-tripped), read from timers/callbacks
  // instead of the state itself so those reads never see a value
  // stale-captured at the point the callback was created — used as the merge
  // base when building the next payload to send.
  const settingsRef = useRef<Settings | null>(null);
  // The last value the backend actually confirmed saving. Distinct from
  // `settingsRef`: on a failed commit, this is what gets restored, never the
  // optimistic value that just failed to persist.
  const lastConfirmedRef = useRef<Settings | null>(null);

  useEffect(() => {
    mounted.current = true;
    getSettings().then((result) => {
      if (!mounted.current) return;
      setSettingsState(result.data);
      settingsRef.current = result.data;
      lastConfirmedRef.current = result.data;
      setLive(result.live);
      setLoading(false);
    });
    return () => {
      mounted.current = false;
      if (commitTimer.current != null) clearTimeout(commitTimer.current);
    };
  }, []);

  /** Sends `next` to the backend and, on success, adopts the (possibly clamped) value it actually saved. */
  const commit = useCallback(async (next: Settings) => {
    try {
      const saved = await setSettings(next);
      if (!mounted.current) return;
      setSettingsState(saved);
      settingsRef.current = saved;
      lastConfirmedRef.current = saved;
      setDraftThreshold(null);
      setSaveError(null);
    } catch (err) {
      if (!mounted.current) return;
      // Revert to the last CONFIRMED value — an optimistic update here would
      // show a setting as saved when it was not.
      setSettingsState(lastConfirmedRef.current);
      settingsRef.current = lastConfirmedRef.current;
      setDraftThreshold(null);
      setSaveError(err instanceof IpcError ? err.message : "Couldn't save settings.");
    }
  }, []);

  /** Immediate controls (toggle/segmented): no clamping applies to these fields, so the sent value is safe to show right away. */
  const commitField = useCallback(
    <K extends keyof Settings>(key: K, value: Settings[K]) => {
      const prev = settingsRef.current;
      if (!prev) return;
      const next = { ...prev, [key]: value };
      setSettingsState(next);
      settingsRef.current = next;
      void commit(next);
    },
    [commit],
  );

  const commitThreshold = useCallback(
    (value: number) => {
      setDraftThreshold(value);
      if (commitTimer.current != null) clearTimeout(commitTimer.current);
      commitTimer.current = setTimeout(() => {
        const prev = settingsRef.current;
        if (!prev) return;
        void commit({ ...prev, threshold: value });
      }, SLIDER_COMMIT_MS);
    },
    [commit],
  );

  if (loading || !settings) {
    return (
      <div className="pane">
        <div className="pane-head">
          <h3>Settings</h3>
        </div>
        <span className="sub">Loading…</span>
      </div>
    );
  }

  const threshold = draftThreshold ?? settings.threshold;
  const fillPct = ((threshold - THRESHOLD_MIN) / (THRESHOLD_MAX - THRESHOLD_MIN)) * 100;
  const grace = graceBucket(settings.graceSeconds);

  return (
    <div className="pane">
      <div className="pane-head">
        <h3>Settings</h3>
        {!live && <span className="sub">Sample settings — not running in the desktop app, so nothing here persists.</span>}
      </div>

      {(saveError || themeError) && (
        <div className="banner danger" role="alert">
          <span>{saveError ?? themeError}</span>
        </div>
      )}

      <div>
        <div className="field">
          <div className="k">
            Theme
            <i>Day, night, or match the system.</i>
          </div>
          <div className="v">
            <Segmented ariaLabel="Theme" value={theme} onChange={onThemeChange} options={THEME_OPTIONS} />
          </div>
        </div>

        <div className="field">
          <div className="k">
            Usage checks
            <i>
              Anthropic limits how often usage can be read — roughly 30 checks an hour per account — so the cadence
              is fixed rather than configurable: about every 5 minutes, tightening automatically as an account nears
              its limit and backing off after a rate limit. Press Refresh on the Accounts screen for a reading right
              now.
            </i>
          </div>
          <div className="v">
            <span className="muted">Every 5 min, adaptive</span>
          </div>
        </div>

        <div className="field">
          <div className="k">
            Auto-switch
            <i>Move to another account before a limit lands.</i>
          </div>
          <div className="v">
            <Toggle
              checked={settings.autoSwitchEnabled}
              onChange={(v) => commitField("autoSwitchEnabled", v)}
              label={settings.autoSwitchEnabled ? "Enabled" : "Disabled"}
            />
          </div>
        </div>

        <div className="field">
          <div className="k">
            Threshold
            <i>Utilisation that triggers a switch.</i>
          </div>
          <div className="v">
            <div className="slider">
              <input
                type="range"
                className="slider-input"
                min={THRESHOLD_MIN}
                max={THRESHOLD_MAX}
                step={1}
                value={threshold}
                onChange={(e) => commitThreshold(Number(e.target.value))}
                style={{
                  background: `linear-gradient(to right, var(--muted) ${fillPct}%, var(--raised) ${fillPct}%)`,
                }}
                aria-label="Auto-switch threshold"
              />
              <span className="num" style={{ fontSize: 13 }}>
                {threshold}%
              </span>
            </div>
          </div>
        </div>

        <div className="field">
          <div className="k">
            Strategy
            <i>How the next account is chosen.</i>
          </div>
          <div className="v">
            <Segmented
              ariaLabel="Auto-switch strategy"
              value={settings.strategy}
              onChange={(v) => commitField("strategy", v)}
              options={STRATEGY_OPTIONS}
            />
          </div>
        </div>

        <div className="field">
          <div className="k">
            Grace period
            <i>Time to intervene before switching.</i>
          </div>
          <div className="v">
            <Segmented
              ariaLabel="Grace period"
              value={grace}
              onChange={(id) => commitField("graceSeconds", GRACE_SECONDS[id])}
              options={GRACE_OPTIONS}
            />
          </div>
        </div>

        <div className="field">
          <div className="k">
            Notify me
            <i>Desktop notifications.</i>
          </div>
          <div className="v">
            <Toggle
              checked={settings.notifyOnSwitch}
              onChange={(v) => commitField("notifyOnSwitch", v)}
              label="When an account is switched"
            />
            <Toggle
              checked={settings.notifyOnExhausted}
              onChange={(v) => commitField("notifyOnExhausted", v)}
              label="When all accounts are exhausted"
            />
            <Toggle
              checked={settings.notifyOnExpiry}
              onChange={(v) => commitField("notifyOnExpiry", v)}
              label="When a login expires"
            />
          </div>
        </div>

        <div className="field">
          <div className="k">
            Start at login
            <i>Run in the tray when the machine starts.</i>
          </div>
          <div className="v">
            <Toggle
              checked={settings.startAtLogin}
              onChange={(v) => commitField("startAtLogin", v)}
              label={settings.startAtLogin ? "Enabled" : "Disabled"}
            />
          </div>
        </div>

        <div className="field">
          <div className="k">
            Account store
            <i>Where your saved logins are kept.</i>
          </div>
          <div className="v">
            <span style={{ fontSize: 12, color: "var(--muted)", maxWidth: "52ch" }}>
              This app keeps your accounts in its own folder, so a fault in either this app
              or the <span className="num">cswap</span> CLI can only affect its own store.
              Switching still installs the chosen login into Claude Code&apos;s official
              location, which is the only thing the two tools share.
            </span>
          </div>
        </div>
      </div>
    </div>
  );
}
