import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  CHECK_INTERVAL_MS,
  checkForUpdate,
  dueForAutoCheck,
  installBlockedBy,
  recordAutoCheck,
  recordNotified,
  shouldNotify,
} from "./updater";

/**
 * The updater's contract with the UI is that it never throws — every outcome is
 * a value the caller renders. These cover the two paths that do not need a real
 * Tauri runtime: no runtime at all, and a runtime whose check fails.
 */

const TAURI_KEY = "__TAURI_INTERNALS__";

function withTauri(internals: unknown) {
  Object.defineProperty(window, TAURI_KEY, {
    value: internals,
    configurable: true,
    writable: true,
  });
}

afterEach(() => {
  if (TAURI_KEY in window) {
    delete (window as unknown as Record<string, unknown>)[TAURI_KEY];
  }
  vi.resetModules();
  vi.unstubAllGlobals();
});

describe("checkForUpdate", () => {
  it("reports unsupported in a plain browser rather than throwing", async () => {
    expect(TAURI_KEY in window).toBe(false);

    // No import of the plugin should even be attempted without a runtime, so a
    // browser preview cannot produce a misleading network-looking error.
    await expect(checkForUpdate()).resolves.toEqual({ kind: "unsupported" });
  });

  it("turns a failing check into a value, not an exception", async () => {
    withTauri({});
    vi.doMock("@tauri-apps/plugin-updater", () => ({
      check: () => Promise.reject(new Error("no internet")),
    }));

    const { checkForUpdate: fresh } = await import("./updater");
    const result = await fresh();

    expect(result.kind).toBe("failed");
    expect(result).toMatchObject({ message: "no internet" });
  });

  /*
   * The plugin says the same thing for a malformed manifest and for a 404,
   * and a 404 is what a repository looks like when its newest published
   * release predates the updater. Reported verbatim, an ordinary state reads
   * as a broken client.
   */
  it("explains a missing manifest instead of quoting the parser", async () => {
    withTauri({});
    vi.doMock("@tauri-apps/plugin-updater", () => ({
      check: () => Promise.reject(new Error("Could not fetch a valid release JSON from the remote")),
    }));

    const { checkForUpdate: fresh } = await import("./updater");
    const result = await fresh();

    expect(result).toMatchObject({ kind: "failed" });
    expect((result as { message: string }).message).toMatch(/no release with update information/i);
    expect((result as { message: string }).message).not.toMatch(/release JSON/i);
  });

  it("names a connection failure as one", async () => {
    withTauri({});
    vi.doMock("@tauri-apps/plugin-updater", () => ({
      check: () => Promise.reject(new Error("error sending request for url")),
    }));

    const { checkForUpdate: fresh } = await import("./updater");
    expect((await fresh() as { message: string }).message).toMatch(/couldn't reach the update server/i);
  });

  it("says plainly when a signature fails, since nothing was installed", async () => {
    withTauri({});
    vi.doMock("@tauri-apps/plugin-updater", () => ({
      check: () => Promise.reject(new Error("minisign signature verification failed")),
    }));

    const { checkForUpdate: fresh } = await import("./updater");
    expect((await fresh() as { message: string }).message).toMatch(/signature did not verify/i);
  });

  it("passes an unrecognised error through rather than guessing at it", async () => {
    withTauri({});
    vi.doMock("@tauri-apps/plugin-updater", () => ({
      check: () => Promise.reject(new Error("disk quota exceeded")),
    }));

    const { checkForUpdate: fresh } = await import("./updater");
    expect((await fresh() as { message: string }).message).toBe("disk quota exceeded");
  });

  it("reports current when the backend says there is no update", async () => {
    withTauri({});
    vi.doMock("@tauri-apps/plugin-updater", () => ({
      check: () => Promise.resolve(null),
    }));

    const { checkForUpdate: fresh } = await import("./updater");
    await expect(fresh()).resolves.toEqual({ kind: "current" });
  });

  it("surfaces the version and trims empty release notes to null", async () => {
    withTauri({});
    const update = { version: "0.2.0", body: "   " };
    vi.doMock("@tauri-apps/plugin-updater", () => ({
      check: () => Promise.resolve(update),
    }));

    const { checkForUpdate: fresh } = await import("./updater");
    const result = await fresh();

    expect(result).toMatchObject({ kind: "available", version: "0.2.0", notes: null });
  });
});

describe("installBlockedBy", () => {
  it("refuses while credentials are mid-rotation", () => {
    // Installing restarts the app. Doing that during a switch means relying on
    // the journal to recover from a crash we chose to cause.
    expect(installBlockedBy({ kind: "switching", from: 0, to: 1 })).toMatch(/in progress/i);
  });

  it("refuses during the pre-switch countdown", () => {
    expect(
      installBlockedBy({ kind: "warning", from: 0, to: 1, deadline: "2026-01-01T00:00:00Z" }),
    ).toMatch(/about to run/i);
  });

  it("refuses until recovery has completed", () => {
    expect(installBlockedBy({ kind: "recoveryRequired", detail: "x" })).toMatch(/recovery/i);
  });

  it("allows the ordinary phases and an unknown one", () => {
    expect(installBlockedBy({ kind: "monitoring" })).toBeNull();
    expect(installBlockedBy({ kind: "disabled" })).toBeNull();
    expect(installBlockedBy({ kind: "exhausted", earliestReset: null })).toBeNull();
    expect(installBlockedBy(null)).toBeNull();
  });
});

describe("automatic check bookkeeping", () => {
  beforeEach(() => window.localStorage.clear());

  it("is due when nothing has ever been recorded", () => {
    expect(dueForAutoCheck()).toBe(true);
  });

  it("is not due again inside the interval, and is once it lapses", () => {
    const t0 = 1_000_000_000_000;
    recordAutoCheck(t0);
    expect(dueForAutoCheck(t0 + CHECK_INTERVAL_MS - 1)).toBe(false);
    expect(dueForAutoCheck(t0 + CHECK_INTERVAL_MS)).toBe(true);
  });

  it("recovers from a clock that moved backwards rather than wedging forever", () => {
    const t0 = 1_000_000_000_000;
    recordAutoCheck(t0);
    // A future-dated stamp would otherwise make the difference negative forever.
    expect(dueForAutoCheck(t0 - 60_000)).toBe(true);
  });

  it("announces a version once, not on every check", () => {
    expect(shouldNotify("0.2.0")).toBe(true);
    recordNotified("0.2.0");
    expect(shouldNotify("0.2.0")).toBe(false);
    // A newer release is still worth saying something about.
    expect(shouldNotify("0.3.0")).toBe(true);
  });
});
