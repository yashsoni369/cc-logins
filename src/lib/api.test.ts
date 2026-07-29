import { emit } from "@tauri-apps/api/event";
import { mockIPC } from "@tauri-apps/api/mocks";
import { describe, expect, it, vi } from "vitest";

import {
  getDaemonStatus,
  getSettingsSnapshot,
  IpcError,
  onDaemonStatusUpdated,
  onSettingsUpdated,
  reloginAccount,
  resumeAutoSwitch,
  snoozeAutoSwitch,
  updateSettings,
} from "@/lib/api";
import type { DaemonStatus, SettingsSnapshot } from "@/types";

const settingsSnapshot: SettingsSnapshot = {
  revision: 4,
  settings: {
    autoSwitchEnabled: true,
    autoSwitchPausedUntil: null,
    threshold: 80,
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

const daemonStatus: DaemonStatus = {
  revision: 7,
  policyRevision: 4,
  phase: { kind: "monitoring" },
  updatedAt: "2026-07-28T12:00:00Z",
};

describe("runtime IPC contracts", () => {
  it("hydrates revisioned settings and daemon status", async () => {
    const calls: Array<[string, unknown]> = [];
    mockIPC((command, args) => {
      calls.push([command, args]);
      if (command === "get_settings") return settingsSnapshot;
      if (command === "get_daemon_status") return daemonStatus;
      return undefined;
    });

    await expect(getSettingsSnapshot()).resolves.toEqual({ data: settingsSnapshot, live: true });
    await expect(getDaemonStatus()).resolves.toEqual({ data: daemonStatus, live: true });
    expect(calls.map(([command]) => command)).toEqual(["get_settings", "get_daemon_status"]);
  });

  it("sends revisioned patches and specialized pause commands with exact arguments", async () => {
    const calls: Array<[string, unknown]> = [];
    mockIPC((command, args) => {
      calls.push([command, args]);
      return settingsSnapshot;
    });

    await updateSettings(4, { threshold: 81 });
    await snoozeAutoSwitch(3600);
    await resumeAutoSwitch();

    expect(calls).toEqual([
      [
        "update_settings",
        { input: { expectedRevision: 4, patch: { threshold: 81 } } },
      ],
      ["snooze_auto_switch", { input: { durationSeconds: 3600 } }],
      ["resume_auto_switch", {}],
    ]);
  });

  it("re-authenticates the selected existing account", async () => {
    const calls: Array<[string, unknown]> = [];
    mockIPC((command, args) => {
      calls.push([command, args]);
      return { schemaVersion: 1, environments: [] };
    });

    await reloginAccount(7);

    expect(calls).toEqual([["relogin_account", { accountNumber: 7 }]]);
  });

  it("preserves structural settings conflict detail", async () => {
    mockIPC(() => {
      throw {
        kind: "settingsConflict",
        detail: { expectedRevision: 3, actualRevision: 4 },
      };
    });

    const error = await updateSettings(3, { graceSeconds: 10 }).catch((reason) => reason);

    expect(error).toBeInstanceOf(IpcError);
    expect(error).toMatchObject({
      kind: "settingsConflict",
      expectedRevision: 3,
      actualRevision: 4,
    });
  });

  it("subscribes to global runtime events and unlistens cleanly", async () => {
    mockIPC(() => undefined, { shouldMockEvents: true });
    const settingsHandler = vi.fn();
    const daemonHandler = vi.fn();
    const unlistenSettings = await onSettingsUpdated(settingsHandler);
    const unlistenDaemon = await onDaemonStatusUpdated(daemonHandler);

    await emit("settings://updated", settingsSnapshot);
    await emit("daemon://status", daemonStatus);
    expect(settingsHandler).toHaveBeenCalledWith(settingsSnapshot);
    expect(daemonHandler).toHaveBeenCalledWith(daemonStatus);

    unlistenSettings();
    unlistenDaemon();
    await emit("settings://updated", { ...settingsSnapshot, revision: 5 });
    await emit("daemon://status", { ...daemonStatus, revision: 8 });
    expect(settingsHandler).toHaveBeenCalledTimes(1);
    expect(daemonHandler).toHaveBeenCalledTimes(1);
  });
});
