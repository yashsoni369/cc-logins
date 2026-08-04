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
 * How often due-ness is *evaluated*. Deliberately far shorter than
 * `CHECK_INTERVAL_MS`, which is the rule about when a check may actually run.
 *
 * They used to be the same value, and a timer that asks "has a day passed?"
 * exactly once a day loses the race with itself. The startup check records its
 * timestamp 30s after launch, so the first daily tick arrives 30s *early* and
 * is refused — pushing the second check out to 48 hours. Later ticks landed
 * precisely on the boundary, where any drift, a suspended laptop or a throttled
 * background timer cost another full day.
 *
 * Polling hourly against a daily gate means no single missed or early tick can
 * delay a check by more than an hour.
 */
export const AUTO_CHECK_POLL_MS = 60 * 60 * 1000;

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
  | { kind: "available"; version: string; highlights: string[]; update: Update }
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

/**
 * A backend error, said in terms of the situation rather than the step that
 * failed.
 *
 * The plugin reports "Could not fetch a valid release JSON from the remote"
 * for anything that stops it parsing a manifest — including a plain 404, which
 * is what a repository whose newest published release predates the updater
 * looks like. That is an ordinary state, not a fault, and passing the wording
 * straight through sends people looking for a broken client.
 *
 * Matching on message text is unavoidable: the plugin surfaces a string, not a
 * typed error. Anything unrecognised falls through unchanged rather than being
 * flattened into a guess.
 */
function describe(error: unknown): string {
  const raw = (error instanceof Error ? error.message : String(error)).trim();
  const lower = raw.toLowerCase();

  if (lower.includes("release json") || lower.includes("404") || lower.includes("not found")) {
    return "No release with update information was found. The newest published release may not carry an update manifest yet.";
  }
  if (
    lower.includes("dns") ||
    lower.includes("sending request") ||
    lower.includes("connect") ||
    lower.includes("timed out") ||
    lower.includes("timeout")
  ) {
    return "Couldn't reach the update server. Check your connection and try again.";
  }
  if (lower.includes("signature") || lower.includes("minisign")) {
    return "The update was downloaded but its signature did not verify, so nothing was installed.";
  }
  return raw || "The update check failed for an unknown reason.";
}

/**
 * The changelog bullets from a release body, as plain sentences.
 *
 * The manifest now carries only the changelog section, but this still trims:
 * every manifest published before that change holds the whole release body,
 * including the install table and checksum instructions written for someone
 * downloading by hand. An app that already downloaded and signature-verified
 * the file must not tell its user to go and do that. Anything after the `---`
 * rule is that boilerplate.
 *
 * Bullets wrap across lines in CHANGELOG.md, so a continuation line is joined
 * back onto its bullet rather than dropped — otherwise every entry would be
 * truncated mid-sentence.
 */
export function releaseHighlights(body: string | null | undefined): string[] {
  if (!body) return [];

  const changelog = body.split(/^\s*---\s*$/m)[0] ?? body;
  const bullets: string[] = [];

  for (const raw of changelog.split("\n")) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    if (line.startsWith("- ")) bullets.push(line.slice(2).trim());
    // A wrapped continuation belongs to the bullet above it. Text before any
    // bullet is a section preamble and is dropped.
    else if (bullets.length) bullets[bullets.length - 1] += ` ${line}`;
  }

  return bullets.filter(Boolean);
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
      highlights: releaseHighlights(update.body),
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
