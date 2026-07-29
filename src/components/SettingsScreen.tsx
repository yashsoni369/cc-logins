import { useCallback, useEffect, useRef, useState } from "react";
import AboutSection from "./AboutSection";
import { Loading } from "./Loading";
import Toggle from "./Toggle";
import type { UseUpdateResult } from "../lib/useUpdate";
import { IpcError } from "../lib/api";
import type { ClockFormat } from "../lib/time";
import type { UseSettingsResult } from "../lib/useSettings";
import type { Theme } from "../lib/useTheme";
import type { Settings } from "../types";

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

const CLOCK_FORMAT_OPTIONS: Array<SegOption<ClockFormat>> = [
  { id: "system", label: "System" },
  { id: "12h", label: "12-hour" },
  { id: "24h", label: "24-hour" },
];

interface SettingsScreenProps {
  runtime: UseSettingsResult;
  /** Update lifecycle, owned by `useUpdate()` in `App.tsx` so the background scheduler and this screen show the same answer. */
  update: UseUpdateResult;
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
 * Uses the window's shared settings owner and persists named-field patches.
 * The threshold slider is debounced so a drag sends one write, not one per
 * pixel; every other control commits immediately since a discrete click is
 * already a single deliberate change. Every commit replaces local state with
 * the backend's *returned* (clamped) settings rather than the value sent —
 * echoing the request would show a number that was not actually saved.
 */
export default function SettingsScreen({
  runtime,
  theme,
  onThemeChange,
  themeError,
  update: updater,
}: SettingsScreenProps) {
  const { settings, live, loading, update } = runtime;
  const [saveError, setSaveError] = useState<string | null>(null);

  // Local draft for the slider only, so dragging feels instant even though
  // the backend write is debounced. Cleared once the backend echoes back.
  const [draftThreshold, setDraftThreshold] = useState<number | null>(null);

  const commitTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const mounted = useRef(true);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      if (commitTimer.current != null) clearTimeout(commitTimer.current);
    };
  }, []);

  /** Sends only named fields; the shared owner adopts the canonical response. */
  const commit = useCallback(
    async (patch: Partial<Settings>) => {
      try {
        await update(patch);
        if (!mounted.current) return;
        setDraftThreshold(null);
        setSaveError(null);
      } catch (err) {
        if (!mounted.current) return;
        setDraftThreshold(null);
        setSaveError(err instanceof IpcError ? err.message : "Couldn't save settings.");
      }
    },
    [update],
  );

  /** Immediate controls (toggle/segmented): no clamping applies to these fields, so the sent value is safe to show right away. */
  const commitField = useCallback(
    <K extends keyof Settings>(key: K, value: Settings[K]) => {
      void commit({ [key]: value } as Pick<Settings, K>);
    },
    [commit],
  );

  const commitThreshold = useCallback(
    (value: number) => {
      setDraftThreshold(value);
      if (commitTimer.current != null) clearTimeout(commitTimer.current);
      commitTimer.current = setTimeout(() => {
        void commit({ threshold: value });
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
        <Loading />
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

      {(saveError || themeError || runtime.error != null) && (
        <div className="banner danger" role="alert">
          <span>{saveError ?? themeError ?? "Couldn't load confirmed settings."}</span>
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
            Time format
            <i>How reset and measurement times are shown.</i>
          </div>
          <div className="v">
            <Segmented
              ariaLabel="Time format"
              value={settings.clockFormat}
              onChange={(v) => commitField("clockFormat", v)}
              options={CLOCK_FORMAT_OPTIONS}
            />
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

        <div className="field">
          <div className="k">
            Check for updates automatically
            <i>Asks GitHub once a day. The only request this app makes outside Anthropic.</i>
          </div>
          <div className="v">
            <Toggle
              checked={settings.autoCheckUpdates}
              onChange={(v) => commitField("autoCheckUpdates", v)}
              label={settings.autoCheckUpdates ? "Enabled" : "Disabled"}
            />
            <span style={{ fontSize: 12, color: "var(--muted)", maxWidth: "52ch" }}>
              Only the current version is sent, and nothing is installed without you asking.
              Turning this off leaves the manual check below working.
            </span>
          </div>
        </div>
      </div>

      <AboutSection update={updater} />
    </div>
  );
}
