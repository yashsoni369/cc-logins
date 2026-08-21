import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import type { SettingsSnapshot } from "@/types";

const api = vi.hoisted(() => ({
  getSettingsSnapshot: vi.fn(),
  onSettingsUpdated: vi.fn(),
  updateSettings: vi.fn(),
  snoozeAutoSwitch: vi.fn(),
  resumeAutoSwitch: vi.fn(),
}));

vi.mock("@/lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/lib/api")>()),
  ...api,
}));

import { IpcError } from "@/lib/api";
import { useSettings } from "@/lib/useSettings";

function snapshot(revision: number, threshold = 90): SettingsSnapshot {
  return {
    revision,
    settings: {
      autoSwitchEnabled: false,
      autoSwitchPausedUntil: null,
      threshold,
      cooldownSeconds: 300,
      hysteresisPct: 10,
      unhealthyTicks: 3,
      strategy: "most-headroom",
      graceSeconds: 60,
      notifyOnSwitch: true,
      notifyOnExhausted: true,
      notifyOnExpiry: false,
      startAtLogin: false,
      autoCheckUpdates: true,
      historyRetentionDays: 14,
      theme: "system",
      clockFormat: "system",
      claudeBinaryPath: null,
    },
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

describe("useSettings", () => {
  let eventHandler: ((value: SettingsSnapshot) => void) | undefined;
  let unlisten: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    eventHandler = undefined;
    unlisten = vi.fn();
    api.onSettingsUpdated.mockImplementation(async (handler) => {
      eventHandler = handler;
      return unlisten;
    });
  });

  it("subscribes before hydration and keeps the highest revision", async () => {
    const hydration = deferred<{ data: SettingsSnapshot; live: boolean }>();
    api.getSettingsSnapshot.mockReturnValue(hydration.promise);
    const { result } = renderHook(() => useSettings());
    await waitFor(() => expect(eventHandler).toBeTypeOf("function"));

    act(() => eventHandler?.(snapshot(2, 82)));
    hydration.resolve({ data: snapshot(1, 70), live: true });

    await waitFor(() => expect(result.current.snapshot?.revision).toBe(2));
    expect(result.current.settings?.threshold).toBe(82);
  });

  it("ignores older events and unlistens on unmount", async () => {
    api.getSettingsSnapshot.mockResolvedValue({ data: snapshot(5, 85), live: true });
    const { result, unmount } = renderHook(() => useSettings());
    await waitFor(() => expect(result.current.snapshot?.revision).toBe(5));

    act(() => eventHandler?.(snapshot(4, 40)));
    expect(result.current.settings?.threshold).toBe(85);
    unmount();
    expect(unlisten).toHaveBeenCalledOnce();
  });

  it("uses the latest accepted revision for writes and serializes rapid patches", async () => {
    api.getSettingsSnapshot.mockResolvedValue({ data: snapshot(4, 84), live: true });
    api.updateSettings
      .mockResolvedValueOnce(snapshot(6, 86))
      .mockResolvedValueOnce({
        ...snapshot(7, 86),
        settings: { ...snapshot(7, 86).settings, graceSeconds: 15 },
      });
    const { result } = renderHook(() => useSettings());
    await waitFor(() => expect(result.current.snapshot?.revision).toBe(4));
    act(() => eventHandler?.(snapshot(5, 85)));

    await act(async () => {
      await Promise.all([result.current.update({ threshold: 86 }), result.current.update({ graceSeconds: 15 })]);
    });

    expect(api.updateSettings).toHaveBeenNthCalledWith(1, 5, { threshold: 86 });
    expect(api.updateSettings).toHaveBeenNthCalledWith(2, 6, { graceSeconds: 15 });
    expect(result.current.snapshot?.revision).toBe(7);
  });

  it("rehydrates confirmed state after a typed conflict", async () => {
    api.getSettingsSnapshot
      .mockResolvedValueOnce({ data: snapshot(2, 82), live: true })
      .mockResolvedValueOnce({ data: snapshot(3, 83), live: true });
    api.updateSettings.mockRejectedValue(
      new IpcError("settingsConflict", { expectedRevision: 2, actualRevision: 3 }),
    );
    const { result } = renderHook(() => useSettings());
    await waitFor(() => expect(result.current.snapshot?.revision).toBe(2));

    await act(async () => {
      await expect(result.current.update({ threshold: 40 })).rejects.toMatchObject({
        kind: "settingsConflict",
      });
    });

    expect(result.current.snapshot?.revision).toBe(3);
    expect(result.current.settings?.threshold).toBe(83);
  });
});
