import { act, renderHook, waitFor } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import { useTheme } from "@/lib/useTheme";
import type { UseSettingsResult } from "@/lib/useSettings";
import type { SettingsSnapshot } from "@/types";

const confirmed: SettingsSnapshot = {
  revision: 3,
  settings: {
    autoSwitchEnabled: false,
    autoSwitchPausedUntil: null,
    threshold: 90,
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
  },
};

function owner(
  update = vi.fn().mockResolvedValue(confirmed),
  snapshot: SettingsSnapshot = confirmed,
): UseSettingsResult {
  return {
    snapshot,
    settings: snapshot.settings,
    live: true,
    loading: false,
    error: null,
    update,
    snooze: vi.fn(),
    resume: vi.fn(),
  };
}

describe("useTheme", () => {
  it("persists only the theme field through the shared settings owner", async () => {
    const update = vi.fn().mockResolvedValue({
      ...confirmed,
      revision: 4,
      settings: { ...confirmed.settings, theme: "night" },
    });
    const { result } = renderHook(() => useTheme(owner(update)));

    act(() => result.current.setTheme("night"));

    expect(result.current.theme).toBe("night");
    await waitFor(() => expect(update).toHaveBeenCalledWith({ theme: "night" }));
  });

  it("restores the confirmed theme when a conflict rehydrates the owner", () => {
    const first = owner();
    const { result, rerender } = renderHook(({ settings }) => useTheme(settings), {
      initialProps: { settings: first },
    });
    act(() => result.current.setTheme("night"));

    rerender({ settings: owner(vi.fn(), { ...confirmed, revision: 4 }) });

    expect(result.current.theme).toBe("system");
  });
});
