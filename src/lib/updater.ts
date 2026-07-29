/**
 * Update checks, kept behind the same kind of seam as `api.ts`.
 *
 * The check is **user-initiated only**. Nothing in this module runs on a timer
 * or at startup, because the README promises no telemetry and no phoning home,
 * and a silent check on every launch would quietly make that false.
 *
 * The payload is verified against the public key baked into `tauri.conf.json`
 * before anything executes. That signature is independent of Authenticode: the
 * installers themselves are unsigned and warned about on first run, but an
 * update arrives through the app rather than a browser, so it carries no mark
 * of the web and never reaches SmartScreen at all.
 *
 * ## Running without Tauri
 *
 * `pnpm dev` in a browser has no updater plugin. Rather than throwing, the
 * check reports `unsupported` so the UI can say so plainly instead of showing
 * an error that looks like a failed network call.
 */

import type { Update } from "@tauri-apps/plugin-updater";
import type { DaemonPhase } from "@/types";

/** How long an automatic check waits before it is willing to run again. */
export const CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000;

/**
 * Startup delay before the first automatic check. Long enough that launch, the
 * first poll and any first-run screen are done with; short enough that a user
 * who opens the app and leaves it still learns about an update today.
 */
export const STARTUP_DELAY_MS = 30_000;

const LAST_CHECK_KEY = "cc-logins.update.lastAutoCheck";
const NOTIFIED_KEY = "cc-logins.update.notifiedVersion";

/** localStorage is unavailable in some webview configurations; never fatal. */
function readLocal(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

function writeLocal(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    // A lost preference is not worth an error path; the cost is one extra
    // check or one repeated notification.
  }
}

/**
 * Why an update must not be installed right now, or `null` when it is safe.
 *
 * Installing restarts the app. Doing that while credentials are mid-rotation
 * means relying on the switch journal to recover from a crash we chose to
 * cause, when waiting costs nothing.
 */
export function installBlockedBy(phase: DaemonPhase | null): string | null {
  switch (phase?.kind) {
    case "switching":
      return "A switch is in progress — updating now would interrupt it.";
    case "warning":
      return "A switch is about to run. Let it finish, or pause auto-switching first.";
    case "recoveryRequired":
      return "Finish credential recovery before updating.";
    default:
      return null;
  }
}

/** Whether an automatic check is due. Manual checks ignore this entirely. */
export function dueForAutoCheck(now: number = Date.now()): boolean {
  const raw = readLocal(LAST_CHECK_KEY);
  if (!raw) return true;
  const last = Number(raw);
  // A corrupt or future-dated value must not wedge checking forever.
  if (!Number.isFinite(last) || last > now) return true;
  return now - last >= CHECK_INTERVAL_MS;
}

export function recordAutoCheck(now: number = Date.now()): void {
  writeLocal(LAST_CHECK_KEY, String(now));
}

/**
 * Whether this version still needs announcing. Keyed by version, not by time,
 * so the same release is announced once and then never again — an update the
 * user has decided to ignore must not nag on every launch.
 */
export function shouldNotify(version: string): boolean {
  return readLocal(NOTIFIED_KEY) !== version;
}

export function recordNotified(version: string): void {
  writeLocal(NOTIFIED_KEY, version);
}

/** Best-effort OS notification. Silence is the correct failure here. */
export async function notifyUpdate(version: string): Promise<void> {
  if (!hasTauri()) return;
  try {
    const { isPermissionGranted, requestPermission, sendNotification } = await import(
      "@tauri-apps/plugin-notification"
    );
    let granted = await isPermissionGranted();
    if (!granted) granted = (await requestPermission()) === "granted";
    if (!granted) return;

    sendNotification({
      title: `CC Logins ${version} is available`,
      body: "Open Settings → About to install it.",
    });
  } catch {
    // The in-app indicator still shows the update; a failed toast is not worth
    // surfacing as an error.
  }
}

/** Outcome of a check. `unsupported` means there is no Tauri runtime at all. */
export type UpdateCheck =
  | { kind: "current" }
  | { kind: "available"; version: string; notes: string | null; update: Update }
  | { kind: "unsupported" }
  | { kind: "failed"; message: string };

/** Progress of an in-flight download, as a fraction where the total is known. */
export interface DownloadProgress {
  downloaded: number;
  total: number | null;
}

function hasTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

/** Trims a backend error to something a user can act on. */
function describe(error: unknown): string {
  const raw = error instanceof Error ? error.message : String(error);
  return raw.trim() || "The update check failed for an unknown reason.";
}

/**
 * Asks GitHub whether a newer release exists. Never throws — a failed check is
 * a state the UI renders, not an exception it has to catch.
 */
export async function checkForUpdate(): Promise<UpdateCheck> {
  if (!hasTauri()) return { kind: "unsupported" };

  try {
    const { check } = await import("@tauri-apps/plugin-updater");
    const update = await check();
    if (!update) return { kind: "current" };
    return {
      kind: "available",
      version: update.version,
      notes: update.body?.trim() || null,
      update,
    };
  } catch (error) {
    return { kind: "failed", message: describe(error) };
  }
}

/**
 * Downloads, verifies and installs an update, then restarts.
 *
 * On Windows the installer relaunches the app itself, so `relaunch()` is only
 * reached on macOS and Linux; calling it after the installer has already taken
 * over is harmless.
 */
export async function installUpdate(
  update: Update,
  onProgress: (progress: DownloadProgress) => void,
): Promise<{ ok: true } | { ok: false; message: string }> {
  try {
    let downloaded = 0;
    let total: number | null = null;

    await update.downloadAndInstall((event) => {
      switch (event.event) {
        case "Started":
          total = event.data.contentLength ?? null;
          onProgress({ downloaded: 0, total });
          break;
        case "Progress":
          downloaded += event.data.chunkLength;
          onProgress({ downloaded, total });
          break;
        case "Finished":
          onProgress({ downloaded: total ?? downloaded, total });
          break;
      }
    });

    const { relaunch } = await import("@tauri-apps/plugin-process");
    await relaunch();
    return { ok: true };
  } catch (error) {
    return { ok: false, message: describe(error) };
  }
}
