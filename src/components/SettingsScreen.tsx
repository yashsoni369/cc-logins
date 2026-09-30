import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import AboutSection from "./AboutSection";
import { Loading } from "./Loading";
import Toggle from "./Toggle";
import RulePreview from "./settings/RulePreview";
import Segmented, { type SegOption } from "./ui/Segmented";
import type { UseUpdateResult } from "../lib/useUpdate";
import { claudeBinaryStatus, IpcError } from "../lib/api";
import ClaudeCommandSection from "./settings/ClaudeCommandSection";
import { ensureNotificationPermission, useAutostart } from "../lib/notifications";
import { useNow, type ClockFormat } from "../lib/time";
import { useBurnSamples } from "../lib/useBurnSamples";
import type { UseSettingsResult } from "../lib/useSettings";
import type { Theme } from "../lib/useTheme";
import type { ClaudeBinaryStatus, Settings, Snapshot } from "../types";

/** `undefined` while still fetching; `null` once known unavailable (no backend). */
type Loadable<T> = T | null | undefined;

/** Discretised view of `graceSeconds` — the real field is continuous, this control picks among three common values. */
type GracePeriod = "off" | "60s" | "5m";

const GRACE_SECONDS: Record<GracePeriod, number> = { off: 0, "60s": 60, "5m": 300 };

/** Discretised `cooldownSeconds`: how long after a switch before auto-switch may move again. */
type Cooldown = "5m" | "15m" | "1h";

const COOLDOWN_SECONDS: Record<Cooldown, number> = { "5m": 300, "15m": 900, "1h": 3600 };

/** Discretised `historyRetentionDays`: how long detailed readings are kept before being summarised by day. */
type Retention = "7" | "14" | "30" | "90";

/**
 * Nearest option for a value set outside this UI (or by a future screen), so
 * it still displays sensibly.
 */
function nearest<T extends string>(ids: T[], value: number, toNumber: (id: T) => number): T {
  return ids.reduce((best, id) => (Math.abs(toNumber(id) - value) < Math.abs(toNumber(best) - value) ? id : best));
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

const COOLDOWN_OPTIONS: Array<SegOption<Cooldown>> = [
  { id: "5m", label: "5 min" },
  { id: "15m", label: "15 min" },
  { id: "1h", label: "1 hour" },
];

const RETENTION_OPTIONS: Array<SegOption<Retention>> = [
  { id: "7", label: "7 days" },
  { id: "14", label: "14 days" },
  { id: "30", label: "30 days" },
  { id: "90", label: "90 days" },
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

const DISPLAY_OPTIONS: Array<SegOption<"used" | "left">> = [
  { id: "used", label: "Used" },
  { id: "left", label: "Left" },
];

type AlertKey = "notifyOnSwitch" | "notifyOnExhausted" | "notifyOnExpiry";

interface SettingsScreenProps {
  runtime: UseSettingsResult;
  /** Update lifecycle, owned by `useUpdate()` in `App.tsx` so the background scheduler and this screen show the same answer. */
  update: UseUpdateResult;
  /** Current theme preference — owned by `useTheme()` in `App.tsx`, passed down so this control and the theme actually applied to the window never disagree. */
  theme: Theme;
  onThemeChange: (theme: Theme) => void;
  /** Set when the most recent theme save failed. The visual change happens regardless — see `useTheme.ts`. */
  themeError: string | null;
  /** Live accounts, for the auto-switch preview. Null before the first snapshot. */
  snapshot?: Snapshot | null;
}

/**
 * Everything auto-switch does, visible and adjustable — a background process
 * that moves credentials has to be legible — followed by alerts, startup,
 * display, data and the rarely-touched advanced settings.
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
  snapshot = null,
}: SettingsScreenProps) {
  const { settings, live, loading, update } = runtime;
  const [saveError, setSaveError] = useState<string | null>(null);
  const [alertNote, setAlertNote] = useState<string | null>(null);

  // Local draft for the slider only, so dragging feels instant even though
  // the backend write is debounced. Cleared once the backend echoes back.
  const [draftThreshold, setDraftThreshold] = useState<number | null>(null);

  // Draft for the claude binary path field. `null` means "not editing" —
  // render the confirmed value. A separate `string | null` from the slider
  // draft above because this field commits on blur/Enter, not a timer.
  const [binaryDraft, setBinaryDraft] = useState<string | null>(null);
  const [binaryStatus, setBinaryStatus] = useState<Loadable<ClaudeBinaryStatus>>(undefined);

  const now = useNow();
  const accounts = useMemo(() => snapshot?.environments.flatMap((e) => e.accounts) ?? [], [snapshot]);
  const { activeSamples } = useBurnSamples(accounts, snapshot);
  const persistStartAtLogin = useCallback((enabled: boolean) => update({ startAtLogin: enabled }), [update]);
  const autostart = useAutostart(persistStartAtLogin);

  const commitTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const mounted = useRef(true);

  const refreshBinaryStatus = useCallback(() => {
    claudeBinaryStatus()
      .then((result) => {
        if (mounted.current) setBinaryStatus(result.data);
      })
      .catch(() => {
        if (mounted.current) setBinaryStatus(null);
      });
  }, []);

  useEffect(() => {
    mounted.current = true;
    refreshBinaryStatus();
    return () => {
      mounted.current = false;
      if (commitTimer.current != null) clearTimeout(commitTimer.current);
    };
  }, [refreshBinaryStatus]);

  /**
   * Sends only named fields; the shared owner adopts the canonical response.
   * Resolves to whether the write actually succeeded, so a caller that needs
   * to react only on success (e.g. re-checking the claude binary status)
   * doesn't have to duplicate the try/catch — the error still lands in the
   * shared `saveError` banner either way.
   */
  const commit = useCallback(
    async (patch: Partial<Settings>): Promise<boolean> => {
      try {
        await update(patch);
        if (!mounted.current) return true;
        setDraftThreshold(null);
        setSaveError(null);
        return true;
      } catch (err) {
        if (!mounted.current) return false;
        setDraftThreshold(null);
        setSaveError(err instanceof IpcError ? err.message : "Couldn't save settings.");
        return false;
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

  /**
   * Turning an alert on is the moment to ask the OS for permission — never at
   * launch. If it is refused the preference is still saved, so it takes
   * effect once allowed, and the refusal is said plainly.
   */
  const commitAlert = useCallback(
    (key: AlertKey, value: boolean) => {
      commitField(key, value);
      if (!value) return;
      void ensureNotificationPermission().then((granted) => {
        if (!mounted.current) return;
        setAlertNote(
          granted || !live
            ? null
            : "Notifications are blocked for CC Logins in your system settings, so alerts won't appear until they're allowed.",
        );
      });
    },
    [commitField, live],
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

  /**
   * Commits the claude binary path on blur/Enter only — deliberately NOT the
   * slider's threshold debounce and NOT a Save button. This is free text: a
   * per-keystroke commit (the debounce approach) would write garbage path
   * prefixes to disk, each one bumping the settings revision, while the user
   * is still mid-edit. Blur/Enter means a value is only ever sent once it
   * looks finished.
   */
  const commitBinaryPath = useCallback(() => {
    setBinaryDraft((draft) => {
      if (draft === null) return null;
      const trimmed = draft.trim();
      const current = settings?.claudeBinaryPath ?? "";
      if (trimmed !== current) {
        void commit({ claudeBinaryPath: trimmed === "" ? null : trimmed }).then((ok) => {
          if (ok) refreshBinaryStatus();
        });
      }
      return null;
    });
  }, [commit, settings, refreshBinaryStatus]);

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
  const grace = nearest<GracePeriod>(["off", "60s", "5m"], settings.graceSeconds, (id) => GRACE_SECONDS[id]);
  const cooldown = nearest<Cooldown>(["5m", "15m", "1h"], settings.cooldownSeconds, (id) => COOLDOWN_SECONDS[id]);
  const retention = nearest<Retention>(["7", "14", "30", "90"], settings.historyRetentionDays, Number);

  return (
    <div className="pane settings">
      <div className="pane-head">
        <h3>Settings</h3>
        {!live && <span className="sub">Sample settings — not running in the desktop app, so nothing here persists.</span>}
      </div>

      {(saveError || themeError || runtime.error != null) && (
        <div className="banner danger" role="alert">
          <span>{saveError ?? themeError ?? "Couldn't load confirmed settings."}</span>
        </div>
      )}

      <section className="settings-group" aria-labelledby="set-auto">
        <h2 id="set-auto">Auto-switch</h2>
        <RulePreview settings={settings} threshold={threshold} accounts={accounts} activeSamples={activeSamples} now={now} />
        <div className="fields">
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
              Don&apos;t switch again for
              <i>After a switch, wait this long before moving again.</i>
            </div>
            <div className="v">
              <Segmented
                ariaLabel="Switch cooldown"
                value={cooldown}
                onChange={(id) => commitField("cooldownSeconds", COOLDOWN_SECONDS[id])}
                options={COOLDOWN_OPTIONS}
              />
            </div>
          </div>
        </div>
      </section>

      <section className="settings-group" aria-labelledby="set-alerts">
        <h2 id="set-alerts">Notifications</h2>
        {alertNote && (
          <div className="banner caution" role="status">
            <span>{alertNote}</span>
          </div>
        )}
        <div className="fields">
          <div className="field">
            <div className="k">
              When auto-switch changes account
              <i>Manual switches aren&apos;t announced — you just made them.</i>
            </div>
            <div className="v">
              <Toggle
                checked={settings.notifyOnSwitch}
                ariaLabel="Notify when auto-switch changes account"
                onChange={(v) => commitAlert("notifyOnSwitch", v)}
              />
            </div>
          </div>
          <div className="field">
            <div className="k">
              When every account is at its limit
              <i>With the earliest reset time.</i>
            </div>
            <div className="v">
              <Toggle
                checked={settings.notifyOnExhausted}
                ariaLabel="Notify when every account is at its limit"
                onChange={(v) => commitAlert("notifyOnExhausted", v)}
              />
            </div>
          </div>
          <div className="field">
            <div className="k">
              When an account needs a fresh sign-in
              <i>Its Claude Code folder is signed out.</i>
            </div>
            <div className="v">
              <Toggle
                checked={settings.notifyOnExpiry}
                ariaLabel="Notify when an account needs a fresh sign-in"
                onChange={(v) => commitAlert("notifyOnExpiry", v)}
              />
            </div>
          </div>
        </div>
      </section>

      <section className="settings-group" aria-labelledby="set-startup">
        <h2 id="set-startup">Startup</h2>
        <div className="fields">
          <div className="field">
            <div className="k">
              Open at login
              <i>Start CC Logins in the tray when you sign in to this computer.</i>
            </div>
            <div className="v">
              <Toggle
                checked={autostart.enabled ?? settings.startAtLogin}
                ariaLabel="Open at login"
                pending={autostart.pending}
                disabled={!live}
                onChange={(v) => void autostart.set(v)}
              />
              {autostart.error && <span className="field-error">{autostart.error}</span>}
            </div>
          </div>
        </div>
      </section>

      <section className="settings-group" aria-labelledby="set-display">
        <h2 id="set-display">Display</h2>
        <div className="fields">
          <div className="field">
            <div className="k">
              Show quota as
              <i>What is used or what is left. Colours always follow how full an account is.</i>
            </div>
            <div className="v">
              <Segmented
                ariaLabel="Show quota as"
                value={settings.displayMode ?? "used"}
                onChange={(v) => commitField("displayMode", v)}
                options={DISPLAY_OPTIONS}
              />
            </div>
          </div>

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
        </div>
      </section>

      <section className="settings-group" aria-labelledby="set-data">
        <h2 id="set-data">Privacy &amp; data</h2>
        <div className="fields">
          <div className="field">
            <div className="k">
              Keep detailed readings for
              <i>Older days are kept as daily summaries, so long-range history stays.</i>
            </div>
            <div className="v">
              <Segmented
                ariaLabel="Keep detailed readings for"
                value={retention}
                onChange={(id) => commitField("historyRetentionDays", Number(id))}
                options={RETENTION_OPTIONS}
              />
            </div>
          </div>

          <div className="field">
            <div className="k">
              Usage checks
              <i>
                Anthropic allows roughly 30 usage reads an hour per account, so checks run about every 5 minutes —
                sooner as an account nears its limit, backing off after a rate limit. Refresh on Home reads now.
              </i>
            </div>
            <div className="v">
              <span className="muted">Every ~5 min, adaptive</span>
            </div>
          </div>

          <div className="field">
            <div className="k">
              Account folders
              <i>Where each account's Claude Code login lives.</i>
            </div>
            <div className="v">
              <span style={{ fontSize: 12, color: "var(--muted)" }}>
                Each account signs in once, through Claude Code itself, into its own folder under ~/.cc-logins/profiles
                (the account signed in to your usual ~/.claude stays there). This app keeps no copy of any login and never
                refreshes one; picking an account only changes which folder new sessions use.
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
              <span style={{ fontSize: 12, color: "var(--muted)" }}>
                Only the current version is sent, and nothing is installed without you asking.
                Turning this off leaves the manual check below working.
              </span>
            </div>
          </div>
        </div>
      </section>

      <ClaudeCommandSection />

      <section className="settings-group" aria-labelledby="set-advanced">
        <h2 id="set-advanced">Advanced</h2>
        <div className="fields">
          <div className="field">
            <div className="k">
              Claude binary
              <i>
                Full path to the claude command, for installs the app can&apos;t find on its own. Apps
                opened from the Dock don&apos;t see PATH changes made in your shell. Leave empty to
                detect automatically.
              </i>
            </div>
            <div className="v">
              <input
                type="text"
                className="input"
                autoComplete="off"
                spellCheck={false}
                aria-label="Claude binary path"
                aria-describedby="claude-binary-status"
                placeholder="Detected automatically"
                value={binaryDraft ?? (settings.claudeBinaryPath ?? "")}
                onChange={(e) => setBinaryDraft(e.target.value)}
                onBlur={commitBinaryPath}
                onKeyDown={(e) => {
                  if (e.key === "Enter") {
                    e.preventDefault();
                    commitBinaryPath();
                  } else if (e.key === "Escape") {
                    e.preventDefault();
                    setBinaryDraft(null);
                  }
                }}
              />
              <span id="claude-binary-status" role="status">
                {binaryStatus?.found && (
                  <span style={{ fontSize: 12, color: "var(--muted)" }}>
                    Found: {binaryStatus.path} ({binaryStatus.source})
                  </span>
                )}
                {binaryStatus && !binaryStatus.found && (
                  <span style={{ fontSize: 12, color: "var(--danger)" }}>{binaryStatus.message}</span>
                )}
              </span>
            </div>
          </div>
        </div>
      </section>

      <AboutSection update={updater} binaryStatus={binaryStatus} />
    </div>
  );
}
