import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import type { DaemonStatus } from "@/types";

const api = vi.hoisted(() => ({
  getDaemonStatus: vi.fn(),
  onDaemonStatusUpdated: vi.fn(),
}));

vi.mock("@/lib/api", () => api);

import { useDaemonStatus } from "@/lib/useDaemonStatus";

function status(revision: number, kind: "disabled" | "monitoring" = "monitoring"): DaemonStatus {
  return {
    revision,
    policyRevision: revision,
    phase: { kind },
    updatedAt: "2026-07-28T12:00:00Z",
  };
}

describe("useDaemonStatus", () => {
  let eventHandler: ((value: DaemonStatus) => void) | undefined;
  let unlisten: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    eventHandler = undefined;
    unlisten = vi.fn();
    api.onDaemonStatusUpdated.mockImplementation(async (handler) => {
      eventHandler = handler;
      return unlisten;
    });
  });

  it("subscribes before hydration and never regresses an event revision", async () => {
    let resolve!: (value: { data: DaemonStatus; live: boolean }) => void;
    api.getDaemonStatus.mockReturnValue(
      new Promise((done) => {
        resolve = done;
      }),
    );
    const { result } = renderHook(() => useDaemonStatus());
    await waitFor(() => expect(eventHandler).toBeTypeOf("function"));

    act(() => eventHandler?.(status(4, "monitoring")));
    resolve({ data: status(3, "disabled"), live: true });

    await waitFor(() => expect(result.current.status?.revision).toBe(4));
    expect(result.current.status?.phase.kind).toBe("monitoring");
  });

  it("ignores older events and unlistens on unmount", async () => {
    api.getDaemonStatus.mockResolvedValue({ data: status(5), live: true });
    const { result, unmount } = renderHook(() => useDaemonStatus());
    await waitFor(() => expect(result.current.status?.revision).toBe(5));

    act(() => eventHandler?.(status(4, "disabled")));
    expect(result.current.status?.phase.kind).toBe("monitoring");
    unmount();
    expect(unlisten).toHaveBeenCalledOnce();
  });
});
