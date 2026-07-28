/**
 * The single seam between the UI and the Rust backend.
 *
 * Nothing else in `src/` may call `invoke` directly. Everything goes through
 * here so there is exactly one place that knows the command names, one place
 * that decodes backend errors, and one place that decides what happens when
 * there is no backend at all.
 *
 * ## Running without Tauri
 *
 * `pnpm dev` serves this app in a plain browser, where the Tauri runtime does
 * not exist and `invoke` is undefined. That is a legitimate way to work on the
 * UI, so instead of throwing, every reader falls back to `mockSnapshot` and
 * flags it. The UI can then say plainly that it is showing sample data rather
 * than quietly presenting fiction as if it were the user's real accounts.
 *
 * Writers do NOT fall back. Pretending a switch succeeded when no backend
 * exists would be a lie about something that touches credentials.
 */

import type { Account, DayStat, Environment, HistorySummary, Settings, Snapshot } from "@/types";
import { stableKey } from "@/types";
import { mockHistoryRanges, mockSnapshot, type HistoryRangeId } from "@/lib/mock";

/** Tagged error from the Rust side. Mirrors `commands::IpcError`. */
export type IpcErrorKind =
  | "notConfigured"
  | "unreachable"
  | "credential"
  | "busy"
  | "internal";

export class IpcError extends Error {
  readonly kind: IpcErrorKind;
  readonly detail?: string;

  constructor(kind: IpcErrorKind, detail?: string) {
    super(detail ? `${kind}: ${detail}` : kind);
    this.name = "IpcError";
    this.kind = kind;
    this.detail = detail;
  }

  /** No accounts are managed yet. A normal first-run state, not a failure. */
  get isNotConfigured() {
    return this.kind === "notConfigured";
  }

  /** A lock is held elsewhere — very likely the `cswap` CLI mid-operation. */
  get isBusy() {
    return this.kind === "busy";
  }
}

function toIpcError(raw: unknown): IpcError {
  if (raw && typeof raw === "object" && "kind" in raw) {
    const { kind, detail } = raw as { kind: IpcErrorKind; detail?: string };
    return new IpcError(kind, detail);
  }
  return new IpcError("internal", String(raw));
}

/** True when running inside the Tauri shell rather than a plain browser. */
export function hasBackend(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  // Imported lazily so a plain-browser session never even loads the Tauri API.
  const { invoke } = await import("@tauri-apps/api/core");
  try {
    return await invoke<T>(command, args);
  } catch (raw) {
    throw toIpcError(raw);
  }
}

/** What a reader returned, and whether it is real. */
export interface Sourced<T> {
  data: T;
  /** False when this is `mock.ts` sample data because no backend was present. */
  live: boolean;
}

// ─── readers ─────────────────────────────────────────────────────────────────
// Safe to call on a timer. These never mutate credential state.

/** Accounts without usage. Fast and offline — paints the first frame. */
export async function getAccounts(): Promise<Sourced<Account[]>> {
  if (!hasBackend()) {
    return { data: mockSnapshot.environments.flatMap((e) => e.accounts), live: false };
  }
  return { data: await call<Account[]>("accounts"), live: true };
}

/**
 * Accounts plus freshly-fetched usage and detected environments.
 *
 * The backend degrades rather than fails: if usage cannot be fetched, accounts
 * still return with last-known values and a stale status, so callers should
 * render what they get rather than treating partial data as an error.
 */
export async function getSnapshot(): Promise<Sourced<Snapshot>> {
  if (!hasBackend()) {
    return { data: mockSnapshot, live: false };
  }
  return { data: await call<Snapshot>("snapshot"), live: true };
}

/**
 * Detected credential realms.
 *
 * Never starts a stopped WSL distro — a stopped one comes back `asleep` with
 * no filesystem access performed, because touching a `\\wsl$` path boots the VM.
 */
export async function getEnvironments(): Promise<Sourced<Environment[]>> {
  if (!hasBackend()) {
    return { data: mockSnapshot.environments, live: false };
  }
  return { data: await call<Environment[]>("environments"), live: true };
}

/** Where the auto-switcher would move right now, without moving. Read-only. */
export async function previewTarget(
  strategy?: "most-headroom" | "next-available" | "consume-first",
): Promise<Account | null> {
  if (!hasBackend()) return null;
  return call<Account | null>("preview_target", { strategy });
}

// ─── events ──────────────────────────────────────────────────────────────
// The Rust poller (`src-tauri/src/poller.rs`) is the single owner of usage
// fetching: it runs one adaptive-cadence loop against the per-token-budgeted
// usage endpoint and pushes results out via Tauri events. Nothing in `src/`
// may start its own polling timer against a usage-fetching command — that
// would spend the same budget the poller's cadence exists to protect a
// second (or third — main window and popover both mount) time.

/** Unsubscribe function returned by an `on*` subscription below. */
export type Unlisten = () => void;

/**
 * Subscribes to `snapshot://updated`, emitted after every successful poller
 * tick with the freshly-fetched `Snapshot`. This is how the UI stays current
 * without polling on its own timer — see `src/lib/useSnapshot.ts`.
 *
 * No-op (resolves to a no-op unlisten) when there is no backend: a plain
 * browser session has no poller emitting anything to subscribe to.
 */
export async function onSnapshotUpdated(handler: (snapshot: Snapshot) => void): Promise<Unlisten> {
  if (!hasBackend()) return () => {};
  const { listen } = await import("@tauri-apps/api/event");
  return listen<Snapshot>("snapshot://updated", (event) => handler(event.payload));
}

// ─── writers ─────────────────────────────────────────────────────────────────
// These change which login Claude Code uses. Never call from a poller, and
// never from a render path — only from an explicit user action.

/**
 * Switch the live login. Returns the post-switch snapshot so the UI never has
 * to guess what happened.
 *
 * Throws rather than falling back when there is no backend: silently reporting
 * success for a credential operation that did not happen would be a lie.
 */
export async function switchAccount(accountNumber: number): Promise<Snapshot> {
  if (!hasBackend()) {
    throw new IpcError(
      "internal",
      "Not running in the desktop app, so accounts cannot be switched.",
    );
  }
  return call<Snapshot>("switch_account", { accountNumber });
}

/**
 * Registers whichever Claude Code login is currently active as a new slot.
 * Needs no interactive auth from this app — the user signs in with Claude
 * Code normally first, and this just captures that live login. Fails (kind
 * `"credential"` in practice) if that login is already registered.
 *
 * Throws rather than falling back when there is no backend, exactly like
 * `switchAccount`: silently reporting a registration that did not happen
 * would be a lie about something that touches credentials.
 */
export async function addCurrentAccount(alias?: string): Promise<Snapshot> {
  if (!hasBackend()) {
    throw new IpcError(
      "internal",
      "Not running in the desktop app, so an account cannot be added.",
    );
  }
  return call<Snapshot>("add_current_account", { alias });
}

/**
 * Opens a real terminal running `claude auth login` against a throwaway
 * config directory, waits for the user to complete the browser OAuth round
 * trip, captures the resulting credential, and registers it as a new
 * account. The user's currently-active login is never touched — closing the
 * terminal at any point cancels safely, with nothing added and nothing else
 * affected.
 *
 * Can fail (via `IpcError.detail`) for several distinct reasons callers
 * should surface differently: the user cancelling by closing the terminal
 * (not an error — return to rest quietly), a 10-minute timeout, `claude` not
 * being on PATH, no terminal emulator being available (Linux only — "Add
 * token" is the fallback there), or the resulting account already being
 * registered.
 *
 * Throws rather than falling back when there is no backend, exactly like
 * `switchAccount` and `addCurrentAccount`: silently reporting a sign-in that
 * did not happen would be a lie about something that touches credentials.
 */
export async function interactiveLogin(alias?: string): Promise<Snapshot> {
  if (!hasBackend()) {
    throw new IpcError(
      "internal",
      "Not running in the desktop app, so a new sign-in cannot be started.",
    );
  }
  return call<Snapshot>("interactive_login", { alias });
}

/**
 * Registers an account from a pasted setup-token or API key rather than a
 * live Claude Code session. The token only ever passes through here on its
 * way to the backend — callers must not log, echo, or hold onto it.
 *
 * Throws rather than falling back when there is no backend, exactly like
 * `switchAccount`.
 */
export async function addToken(token: string, email?: string, alias?: string): Promise<Snapshot> {
  if (!hasBackend()) {
    throw new IpcError(
      "internal",
      "Not running in the desktop app, so a token cannot be added.",
    );
  }
  return call<Snapshot>("add_token", { token, email, alias });
}

/**
 * Holds an account out of (or back into) auto-rotation. Disabling the
 * currently-active account is refused by the backend — callers should
 * surface that refusal specifically rather than as a generic failure.
 *
 * Throws rather than falling back when there is no backend, exactly like
 * `switchAccount`.
 */
export async function setAccountEnabled(accountNumber: number, enabled: boolean): Promise<Snapshot> {
  if (!hasBackend()) {
    throw new IpcError(
      "internal",
      "Not running in the desktop app, so accounts cannot be changed.",
    );
  }
  return call<Snapshot>("set_account_enabled", { accountNumber, enabled });
}


// ─── history (read-only) ───────────────────────────────────────────────────
// Backed by a local SQLite store the desktop app writes to as it polls.
// Never available without a backend, so every reader here degrades to
// `mockHistoryRanges` — the same fixture the History screen used before it
// was wired up — flagged `live: false` like every other sample fallback.

/** Stable keys of `mockSnapshot`'s accounts, in order, computed once and reused. */
let mockAccountKeysPromise: Promise<string[]> | null = null;
function mockAccountKeys(): Promise<string[]> {
  mockAccountKeysPromise ??= Promise.all(
    mockSnapshot.environments.flatMap((e) => e.accounts).map((a) => stableKey(a)),
  );
  return mockAccountKeysPromise;
}

function rangeIdForDays(days: number): HistoryRangeId {
  if (days <= 7) return "7d";
  if (days <= 30) return "30d";
  return "90d";
}

/** Turns one `mockHistoryRanges` account series into `DayStat[]`, dated backward from today. */
function mockDayStats(data: readonly number[]): DayStat[] {
  const today = new Date();
  return data.map((pct, i) => {
    const offset = data.length - 1 - i;
    const d = new Date(Date.UTC(today.getUTCFullYear(), today.getUTCMonth(), today.getUTCDate()));
    d.setUTCDate(d.getUTCDate() - offset);
    return {
      day: d.toISOString().slice(0, 10),
      minPct: pct,
      maxPct: pct,
      avgPct: pct,
      sampleCount: 1,
    };
  });
}

/** Headline figures backing the History screen's stat row. `null` when there is no history yet. */
export async function historySummary(days?: number): Promise<Sourced<HistorySummary | null>> {
  if (!hasBackend()) {
    const range = mockHistoryRanges.find((r) => r.id === rangeIdForDays(days ?? 30));
    const data = range
      ? {
          weeklyAveragePct: range.stats.weeklyAveragePct,
          timesAt100Pct: range.stats.limitsHit,
          busiestWeekday: range.stats.busiestDay,
        }
      : null;
    return { data, live: false };
  }
  return { data: await call<HistorySummary | null>("history_summary", { days }), live: true };
}

/**
 * Per-day min/max/avg for one account, keyed by `stableKey(account)`. Empty
 * when that account has no recorded history yet — a normal state for a
 * freshly added account, not an error.
 */
export async function historySeries(accountKey: string, days?: number): Promise<Sourced<DayStat[]>> {
  if (!hasBackend()) {
    const keys = await mockAccountKeys();
    const index = keys.indexOf(accountKey);
    const range = mockHistoryRanges.find((r) => r.id === rangeIdForDays(days ?? 30));
    const series = index === -1 ? undefined : range?.series[index];
    return { data: series ? mockDayStats(series.data) : [], live: false };
  }
  return { data: await call<DayStat[]>("history_series", { accountKey, days }), live: true };
}

/**
 * Whether the history subsystem is actually up this session. The backend
 * still runs without it (a corrupt or unwritable database must not stop the
 * app from starting), so this can be `false` even with a real backend
 * present — the History screen must be able to say "no history yet" plainly
 * rather than render an empty chart that reads as zero usage.
 */
export async function historyAvailable(): Promise<Sourced<boolean>> {
  if (!hasBackend()) return { data: false, live: false };
  return { data: await call<boolean>("history_available"), live: true };
}

// ─── settings ────────────────────────────────────────────────────────────

/** Mirrors `Settings::default()` in `src-tauri/src/settings.rs`. */
export const DEFAULT_SETTINGS: Settings = {
  autoSwitchEnabled: false,
  threshold: 90,
  intervalSeconds: 60,
  cooldownSeconds: 300,
  hysteresisPct: 10,
  unhealthyTicks: 3,
  strategy: "most-headroom",
  graceSeconds: 60,
  notifyOnSwitch: true,
  notifyOnExhausted: true,
  notifyOnExpiry: false,
  startAtLogin: false,
  historyRetentionDays: 14,
  theme: "system",
};

/** Current settings. Falls back to the same defaults the backend ships with. */
export async function getSettings(): Promise<Sourced<Settings>> {
  if (!hasBackend()) return { data: DEFAULT_SETTINGS, live: false };
  return { data: await call<Settings>("get_settings"), live: true };
}

/**
 * Persist settings. Returns the values the backend actually saved, which are
 * clamped (threshold to 50..99, interval to 15..3600, etc.) and may not
 * match what was sent — callers must render this return value, not their
 * request, or the UI would show a number that was never actually saved.
 *
 * Throws rather than falling back when there is no backend, exactly like
 * `switchAccount`: silently reporting a save that did not happen would be a
 * lie about something the user just explicitly asked to persist.
 */
export async function setSettings(settings: Settings): Promise<Settings> {
  if (!hasBackend()) {
    throw new IpcError("internal", "Not running in the desktop app, so settings cannot be saved.");
  }
  return call<Settings>("set_settings", { settings });
}
