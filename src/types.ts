/**
 * Account and usage shapes.
 *
 * These mirror the `cswap --json` contract (schemaVersion 1) field for field,
 * so this app and the CLI describe the same machine state the same way. The
 * Rust side serialises into exactly these shapes.
 *
 * Rule inherited from that contract: ignore unknown fields rather than
 * rejecting them, so a newer producer does not break an older consumer.
 */

/** One rate-limit window. */
export interface UsageWindow {
  /** Utilisation, 0..100. Not headroom. */
  pct: number;
  /** ISO-8601 instant the window resets, when the API supplied one. */
  resetsAt?: string;
  /** Human countdown, e.g. "4h 21m". Recomputed at render time, never trusted from cache. */
  countdown?: string;
  /** Absolute clock, e.g. "04:49" same-day or "Aug 1 01:29". */
  clock?: string;
}

/** The seven-day window additionally carries pace projection. */
export interface SevenDayWindow extends UsageWindow {
  /** Where utilisation *should* be at this point in the window. */
  expectedPct?: number;
  /** True when burning faster than the window can sustain. */
  aheadOfPace?: boolean;
  /** ISO instant the quota is projected to run out, at the current rate. */
  projectedExhaustionAt?: string;
  /** False means the projection says you run out before the window resets. */
  willLastToReset?: boolean;
}

/** A per-model weekly limit, e.g. `{ name: "Fable", pct: 25 }`. */
export interface ScopedWindow extends SevenDayWindow {
  /** Model display name as reported by the usage API. */
  name: string;
}

export interface Usage {
  fiveHour?: UsageWindow;
  sevenDay?: SevenDayWindow;
  /** Per-model weekly limits. Absent on older responses. */
  scoped?: ScopedWindow[];
}

/**
 * Why an account cannot currently be used or measured.
 *
 * Deliberately an OPEN union. The variant set is whatever the upstream CLI
 * emits, which we do not control — a live differential run caught it emitting
 * `"unavailable"`, a status that appears only during a transient usage-fetch
 * failure and so was absent from every captured fixture. The Rust side
 * degrades an unrecognised status to `"unknown"` rather than failing to parse
 * the account; the `(string & {})` arm keeps this side honest about that while
 * preserving autocomplete on the known variants.
 */
export type UsageStatus =
  | "ok"
  /** Live OAuth bytes were positively attributed to a different account. */
  | "foreigncredential"
  /** The server proved the refresh-token lineage is dead; re-login required. */
  | "reloginrequired"
  /** Legacy producer spelling, accepted for compatibility only. */
  | "expired"
  /** Usage could not be read this cycle; last-known values are shown. */
  | "stale"
  /** Held out of auto-rotation by the user. */
  | "disabled"
  /** Usage could not be retrieved right now. Transient. */
  | "unavailable"
  /** The CLI reported an error state for this account. */
  | "error"
  /** No usage has ever been read, or a status this build does not know. */
  | "unknown"
  // eslint-disable-next-line @typescript-eslint/ban-types
  | (string & {});

export interface Account {
  /** Slot number. Stable, user-reorderable. */
  number: number;
  email: string;
  /** Short user-set name, preferred over email everywhere in the UI. */
  alias?: string;
  organizationName?: string;
  organizationUuid?: string;
  isOrganization?: boolean;
  /** True for the one account whose credentials are currently live. */
  active: boolean;
  usageStatus: UsageStatus;
  usage?: Usage;
  /** ISO instant the usage was measured. */
  usageFetchedAt?: string;
  /** Age of the measurement. Drives the staleness badge. */
  usageAgeSeconds?: number;
}

/** A distinct credential store — native Windows, a WSL distro, or a profile dir. */
export interface Environment {
  id: string;
  /** Display name, e.g. "Windows" or "WSL · Ubuntu". */
  label: string;
  /** The config path this realm resolves to. */
  path: string;
  kind: "native" | "wsl" | "profile";
  /**
   * `live` — readable now.
   * `asleep` — a stopped WSL distro. Reading it would boot the VM, so it is
   *   never polled; values are last-known and waking is an explicit action.
   * `ignored` — no Claude Code install found here.
   */
  status: "live" | "asleep" | "ignored";
  accounts: Account[];
  /** For an asleep realm, how old the last-known reading is. */
  lastSeenSeconds?: number;
  /**
   * Whether Claude Code credentials were found in this realm, independent of
   * `accounts` (which isn't populated for every environment kind yet).
   * `undefined` means "not determined" — e.g. never probed, or probing would
   * have required touching a stopped WSL distro's filesystem, which never
   * happens outside an explicit user-initiated wake. Distinct from `false`,
   * which means the probe ran and found nothing.
   */
  hasCredentials?: boolean;
}

export interface Snapshot {
  schemaVersion: 1;
  activeAccountNumber?: number;
  environments: Environment[];
}

// ── derived helpers ──────────────────────────────────────────────────────────

/** Every window that gates this account, as `[label, pct]`. */
export function bindingWindows(u: Usage | undefined): Array<[string, number]> {
  if (!u) return [];
  const out: Array<[string, number]> = [];
  if (u.fiveHour) out.push(["5h", u.fiveHour.pct]);
  if (u.sevenDay) out.push(["7d", u.sevenDay.pct]);
  for (const s of u.scoped ?? []) out.push([s.name, s.pct]);
  return out;
}

/**
 * Utilisation of the binding window — the highest of them. This is the number
 * the tray shows. Returns null when usage is unknown, which callers must treat
 * as "never auto-skip", never as zero.
 */
export function bindingUtilisation(u: Usage | undefined): number | null {
  const pcts = bindingWindows(u).map(([, p]) => p);
  return pcts.length ? Math.max(...pcts) : null;
}

/** Remaining percentage before the account hits a limit. */
export function headroom(u: Usage | undefined): number | null {
  const b = bindingUtilisation(u);
  return b === null ? null : 100 - b;
}

export type QuotaState = "ok" | "caution" | "danger";

/** Thresholds match the auto-switch defaults and the tray rasteriser. */
export function quotaState(pct: number | null | undefined): QuotaState {
  if (pct == null) return "ok";
  if (pct >= 90) return "danger";
  if (pct >= 75) return "caution";
  return "ok";
}

export type PaceState = "ok" | "caution" | "danger";

/**
 * Ratio of actual to expected 7-day utilisation ("1.1×"), or null when the
 * window is missing an expectation to compare against.
 */
export function paceRatio(sevenDay: SevenDayWindow | undefined): number | null {
  if (!sevenDay || sevenDay.expectedPct == null || sevenDay.expectedPct === 0) return null;
  return sevenDay.pct / sevenDay.expectedPct;
}

/**
 * Colour state for the pace column. The usage API's `aheadOfPace` and
 * `willLastToReset` are authoritative — they come from the server, not our
 * arithmetic — so they drive the colour whenever present. The derived ratio
 * is only used to grade severity when no flag is available at all; a
 * disagreement between our ratio and the server's flag must never win.
 */
export function paceState(sevenDay: SevenDayWindow | undefined, ratio: number | null): PaceState {
  const ahead = sevenDay?.aheadOfPace;
  if (ahead === false) return "ok";
  if (ahead === true) {
    return sevenDay?.willLastToReset === false ? "danger" : "caution";
  }
  // No authoritative flag on this window — fall back to the ratio, and only
  // colour when it is genuinely off-pace.
  if (ratio == null) return "ok";
  if (ratio >= 2.0) return "danger";
  if (ratio >= 1.5) return "caution";
  return "ok";
}

/** Format a measurement age the way the staleness badge reads it. */
export function ageLabel(seconds: number | undefined): string | null {
  if (seconds == null) return null;
  if (seconds < 30) return null; // current enough to say nothing
  if (seconds < 90) return "1m old";
  if (seconds < 3600) return `${Math.round(seconds / 60)}m old`;
  if (seconds < 86400) return `${Math.round(seconds / 3600)}h old`;
  return `${Math.round(seconds / 86400)}d old`;
}

/** Mask an email for display. People screenshot this app. */
export function maskEmail(email: string): string {
  const at = email.indexOf("@");
  if (at <= 0) return email;
  return `${email[0]}•••${email.slice(at)}`;
}

/** Preferred display name: alias, else the masked email's local part. */
export function displayName(a: Account): string {
  return a.alias?.trim() || maskEmail(a.email);
}

/**
 * Stable identity for history rows, mirroring `Account::stable_key()` in
 * `src-tauri/src/model.rs` byte-for-byte: the org UUID when present and
 * non-blank, otherwise `email:{sha256hex}` of the trimmed, lowercased email.
 *
 * Must stay identical to the Rust side or this app would ask the backend for
 * a different account's history than the one on screen. Async because the
 * only standards-based SHA-256 in a browser/webview is `crypto.subtle`,
 * which is Promise-based; there is no synchronous alternative worth adding a
 * dependency for.
 */
export async function stableKey(account: Account): Promise<string> {
  const uuid = account.organizationUuid?.trim();
  if (uuid) return `org:${uuid}`;

  const normalized = account.email.trim().toLowerCase();
  const bytes = new TextEncoder().encode(normalized);
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  const hex = Array.from(new Uint8Array(digest))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
  return `email:${hex}`;
}

// ── settings ──────────────────────────────────────────────────────────────

/** Mirrors `src-tauri/src/settings.rs::Settings`, serde camelCase. */
export interface Settings {
  autoSwitchEnabled: boolean;
  /** Persisted daemon pause deadline, or null when automatic switching is live. */
  autoSwitchPausedUntil: string | null;
  /** 50..99, clamped by the backend on save. */
  threshold: number;
  /** 0..86400 seconds, clamped by the backend on save. */
  cooldownSeconds: number;
  /** 0..50, clamped by the backend on save. */
  hysteresisPct: number;
  /** 1..20, clamped by the backend on save. */
  unhealthyTicks: number;
  strategy: "most-headroom" | "next-available" | "consume-first";
  /** 0..3600 seconds, clamped by the backend on save. */
  graceSeconds: number;
  notifyOnSwitch: boolean;
  notifyOnExhausted: boolean;
  notifyOnExpiry: boolean;
  startAtLogin: boolean;
  /**
   * Whether the app may ask GitHub for a newer release on its own. On by
   * default. The only setting that permits an outbound request to anything
   * other than Anthropic; the manual check in About works regardless.
   */
  autoCheckUpdates: boolean;
  /** 1..3650 days, clamped by the backend on save. */
  historyRetentionDays: number;
  /**
   * Appearance. Default `"system"` — see `src/lib/useTheme.ts` for how this
   * maps onto `data-theme` on `document.documentElement`.
   */
  theme: "system" | "day" | "night";
}

export interface SettingsSnapshot {
  revision: number;
  settings: Settings;
}

export type SettingsPatch = Partial<Settings>;

export type DegradedReason = "usageUnknown" | "fetchFailed";

export type DaemonPhase =
  | { kind: "disabled" }
  | { kind: "paused"; until: string }
  | { kind: "monitoring" }
  | { kind: "cooldown"; until: string }
  | { kind: "warning"; from: number; to: number; deadline: string }
  | { kind: "switching"; from: number; to: number }
  | { kind: "exhausted"; earliestReset: string | null }
  | { kind: "degraded"; reason: DegradedReason }
  | { kind: "recoveryRequired"; detail: string };

export interface DaemonStatus {
  revision: number;
  policyRevision: number;
  phase: DaemonPhase;
  updatedAt: string;
}

// ── about ─────────────────────────────────────────────────────────────────

/**
 * Absolute paths to the files this app owns, mirroring
 * `src-tauri/src/commands.rs::DataLocations`. Always resolved by the backend
 * — never rebuilt here from platform guesses.
 */
export interface DataLocations {
  /** This app's own account vault. */
  accountVault: string;
  /** Settings file and history database. */
  dataDir: string;
  logFile: string;
}

// ── history ───────────────────────────────────────────────────────────────

/**
 * Min/max/avg/count of `bindingUtilisation()` for one account on one
 * calendar day (UTC). Mirrors `src-tauri/src/history.rs::DayStat`. A day with
 * no measurement simply has no entry — callers must not assume every day in
 * a requested range is represented.
 */
export interface DayStat {
  /** `YYYY-MM-DD`, UTC. */
  day: string;
  minPct: number;
  maxPct: number;
  avgPct: number;
  sampleCount: number;
}

/**
 * Headline figures for the History screen's stat row. Mirrors
 * `src-tauri/src/history.rs::HistorySummary`. `busiestWeekday` is `null` when
 * there is no data in the requested window — never fall back to a fabricated
 * default like "Monday".
 */
export interface HistorySummary {
  weeklyAveragePct: number;
  timesAt100Pct: number;
  busiestWeekday: string | null;
}
