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
