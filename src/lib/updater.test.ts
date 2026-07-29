import { afterEach, describe, expect, it, vi } from "vitest";
import { checkForUpdate } from "./updater";

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
