import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import SettingsScreen from "@/components/SettingsScreen";
import type { UseSettingsResult } from "@/lib/useSettings";
import type { UseUpdateResult } from "@/lib/useUpdate";
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
    autoCheckUpdates: true,
    historyRetentionDays: 14,
    theme: "system",
    clockFormat: "system",
    claudeBinaryPath: null,
  },
};

function owner(update: UseSettingsResult["update"], claudeBinaryPath: string | null = null): UseSettingsResult {
  const settings = { ...confirmed.settings, claudeBinaryPath };
  return {
    snapshot: { ...confirmed, settings },
    settings,
    live: true,
    loading: false,
    error: null,
    update,
    snooze: vi.fn(),
    resume: vi.fn(),
  };
}

/** No update found, nothing in flight — the update row must not affect these assertions. */
const noUpdate: UseUpdateResult = {
  status: null,
  checking: false,
  install: { kind: "idle" },
  blocked: null,
  available: false,
  check: async () => {},
  startInstall: async () => {},
};

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
        update={noUpdate}
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

  it("offers a time format choice and saves it as a single field", () => {
    const update = vi.fn().mockResolvedValue(confirmed);
    render(
      <SettingsScreen
        runtime={owner(update)}
        theme="system"
        onThemeChange={vi.fn()}
        themeError={null}
        update={noUpdate}
      />,
    );

    // Scoped: the Theme control has its own "System" option, checked too.
    const group = within(screen.getByRole("radiogroup", { name: "Time format" }));
    expect(group.getByRole("radio", { name: "System", checked: true })).toBeInTheDocument();

    fireEvent.click(group.getByRole("radio", { name: "24-hour" }));
    expect(update).toHaveBeenCalledWith({ clockFormat: "24h" });
  });

  it("does not expose deferred notification or start-at-login controls", () => {
    render(
      <SettingsScreen
        runtime={owner(vi.fn())}
        theme="system"
        onThemeChange={vi.fn()}
        themeError={null}
        update={noUpdate}
      />,
    );

    expect(screen.queryByText("Notify me")).not.toBeInTheDocument();
    expect(screen.queryByText("Start at login")).not.toBeInTheDocument();
  });

  it("commits the trimmed claude binary path on blur", () => {
    const update = vi.fn().mockResolvedValue(confirmed);
    render(
      <SettingsScreen
        runtime={owner(update)}
        theme="system"
        onThemeChange={vi.fn()}
        themeError={null}
        update={noUpdate}
      />,
    );

    const input = screen.getByLabelText("Claude binary path");
    fireEvent.change(input, { target: { value: "  /usr/local/bin/claude  " } });
    fireEvent.blur(input);

    expect(update).toHaveBeenCalledWith({ claudeBinaryPath: "/usr/local/bin/claude" });
  });

  it("enter commits and the following blur does not write again", () => {
    const update = vi.fn().mockResolvedValue(confirmed);
    render(
      <SettingsScreen
        runtime={owner(update)}
        theme="system"
        onThemeChange={vi.fn()}
        themeError={null}
        update={noUpdate}
      />,
    );

    const input = screen.getByLabelText("Claude binary path");
    fireEvent.change(input, { target: { value: "/usr/local/bin/claude" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(update).toHaveBeenCalledTimes(1);
    expect(update).toHaveBeenCalledWith({ claudeBinaryPath: "/usr/local/bin/claude" });

    fireEvent.blur(input);
    expect(update).toHaveBeenCalledTimes(1);
  });

  it("an emptied field commits null to clear the override", () => {
    const update = vi.fn().mockResolvedValue(confirmed);
    render(
      <SettingsScreen
        runtime={owner(update, "/usr/local/bin/claude")}
        theme="system"
        onThemeChange={vi.fn()}
        themeError={null}
        update={noUpdate}
      />,
    );

    const input = screen.getByLabelText("Claude binary path");
    fireEvent.change(input, { target: { value: "" } });
    fireEvent.blur(input);

    expect(update).toHaveBeenCalledWith({ claudeBinaryPath: null });
  });

  it("an unchanged value does not write on blur", () => {
    const update = vi.fn().mockResolvedValue(confirmed);
    render(
      <SettingsScreen
        runtime={owner(update, "/usr/local/bin/claude")}
        theme="system"
        onThemeChange={vi.fn()}
        themeError={null}
        update={noUpdate}
      />,
    );

    const input = screen.getByLabelText("Claude binary path");
    fireEvent.change(input, { target: { value: "  /usr/local/bin/claude  " } });
    fireEvent.blur(input);

    expect(update).not.toHaveBeenCalled();
  });

  it("escape abandons the draft without a write", () => {
    const update = vi.fn().mockResolvedValue(confirmed);
    render(
      <SettingsScreen
        runtime={owner(update, "/usr/local/bin/claude")}
        theme="system"
        onThemeChange={vi.fn()}
        themeError={null}
        update={noUpdate}
      />,
    );

    const input = screen.getByLabelText("Claude binary path") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "/something/else" } });
    fireEvent.keyDown(input, { key: "Escape" });
    fireEvent.blur(input);

    expect(update).not.toHaveBeenCalled();
    expect(input.value).toBe("/usr/local/bin/claude");
  });
});
