import { act, renderHook } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

/**
 * Regression coverage for the notify-gating bug: `recordNotified` must only
 * run once `notifyUpdate` confirms the OS actually sent something, or a
 * denied/failed attempt is marked "announced" forever with no retry.
 */
const updater = vi.hoisted(() => ({
  checkForUpdate: vi.fn(),
  dueForAutoCheck: vi.fn(() => true),
  installBlockedBy: vi.fn(() => null),
  installUpdate: vi.fn(),
  notifyUpdate: vi.fn(),
  recordAutoCheck: vi.fn(),
  recordNotified: vi.fn(),
  shouldNotify: vi.fn(() => true),
  AUTO_CHECK_POLL_MS: 10_000,
  STARTUP_DELAY_MS: 5,
}));

vi.mock("./updater", () => updater);

import { useUpdate } from "./useUpdate";

const AVAILABLE = {
  kind: "available" as const,
  version: "0.3.0",
  highlights: [] as string[],
  update: {} as never,
};

describe("useUpdate notify gating", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    updater.checkForUpdate.mockReset().mockResolvedValue(AVAILABLE);
    updater.dueForAutoCheck.mockReset().mockReturnValue(true);
    updater.shouldNotify.mockReset().mockReturnValue(true);
    updater.installBlockedBy.mockReset().mockReturnValue(null);
    updater.recordAutoCheck.mockReset();
    updater.recordNotified.mockReset();
    updater.notifyUpdate.mockReset();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("does not mark a version notified when the OS notification fails", async () => {
    updater.notifyUpdate.mockResolvedValue(false);

    renderHook(() => useUpdate(true, null));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(updater.STARTUP_DELAY_MS);
    });

    expect(updater.notifyUpdate).toHaveBeenCalledWith("0.3.0");
    expect(updater.recordNotified).not.toHaveBeenCalled();
  });

  it("marks a version notified once the OS notification actually sends", async () => {
    updater.notifyUpdate.mockResolvedValue(true);

    renderHook(() => useUpdate(true, null));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(updater.STARTUP_DELAY_MS);
    });

    expect(updater.recordNotified).toHaveBeenCalledWith("0.3.0");
  });
});
