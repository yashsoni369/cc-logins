import { act, fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import SettingsScreen from "@/components/SettingsScreen";
import type { UseSettingsResult } from "@/lib/useSettings";
import type { SettingsSnapshot } from "@/types";

const confirmed: SettingsSnapshot = {
  revision: 2,
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
    historyRetentionDays: 14,
    theme: "system",
  },
};

function owner(update: UseSettingsResult["update"]): UseSettingsResult {
  return {
    snapshot: confirmed,
    settings: confirmed.settings,
    live: true,
    loading: false,
    error: null,
    update,
    snooze: vi.fn(),
    resume: vi.fn(),
  };
}

describe("SettingsScreen", () => {
  it("sends single-field toggle and debounced threshold patches", async () => {
    vi.useFakeTimers();
    const update = vi.fn().mockResolvedValue(confirmed);
    render(
      <SettingsScreen
        runtime={owner(update)}
        theme="system"
        onThemeChange={vi.fn()}
        themeError={null}
      />,
    );

    fireEvent.click(screen.getByRole("switch", { name: /disabled/i }));
    expect(update).toHaveBeenCalledWith({ autoSwitchEnabled: true });

    fireEvent.change(screen.getByRole("slider", { name: "Auto-switch threshold" }), {
      target: { value: "81" },
    });
    expect(update).not.toHaveBeenCalledWith({ threshold: 81 });
    await act(async () => vi.advanceTimersByTime(400));
    expect(update).toHaveBeenCalledWith({ threshold: 81 });
    vi.useRealTimers();
  });

  it("does not expose deferred notification or start-at-login controls", () => {
    render(
      <SettingsScreen
        runtime={owner(vi.fn())}
        theme="system"
        onThemeChange={vi.fn()}
        themeError={null}
      />,
    );

    expect(screen.queryByText("Notify me")).not.toBeInTheDocument();
    expect(screen.queryByText("Start at login")).not.toBeInTheDocument();
  });
});
