import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

import type { DaemonStatus, Snapshot } from "@/types";

const mocks = vi.hoisted(() => ({
  status: null as DaemonStatus | null,
  switchAccount: vi.fn().mockResolvedValue(undefined),
  refresh: vi.fn().mockResolvedValue(undefined),
  snooze: vi.fn().mockResolvedValue(undefined),
  resume: vi.fn().mockResolvedValue(undefined),
}));

vi.mock("@/lib/useDaemonStatus", () => ({
  useDaemonStatus: () => ({ status: mocks.status, live: true, loading: false, error: null }),
}));
vi.mock("@/lib/useSnapshot", () => ({
  useSnapshot: () => ({
    snapshot: fixture,
    live: true,
    loading: false,
    error: null,
    refresh: mocks.refresh,
  }),
}));
vi.mock("@/lib/useSettings", () => ({
  useSettings: () => ({
    snapshot: null,
    settings: null,
    live: true,
    loading: false,
    error: null,
    update: vi.fn(),
    snooze: mocks.snooze,
    resume: mocks.resume,
  }),
}));
vi.mock("@/lib/useTheme", () => ({ useTheme: vi.fn() }));
vi.mock("@/lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/lib/api")>()),
  hasBackend: () => false,
  switchAccount: mocks.switchAccount,
}));

import PopoverPanel from "@/components/PopoverPanel";

const fixture: Snapshot = {
  schemaVersion: 1,
  activeAccountNumber: 1,
  environments: [
    {
      id: "native",
      label: "Native",
      path: "",
      kind: "native",
      status: "live",
      accounts: [
        {
          number: 1,
          email: "active@example.com",
          alias: "Active",
          active: true,
          usageStatus: "ok",
          usage: { sevenDay: { pct: 99 } },
        },
        {
          number: 2,
          email: "next@example.com",
          alias: "Next",
          active: false,
          usageStatus: "ok",
          usage: { sevenDay: { pct: 10 } },
        },
      ],
    },
  ],
};

function status(phase: DaemonStatus["phase"], revision = 1): DaemonStatus {
  return {
    revision,
    policyRevision: revision,
    phase,
    updatedAt: "2026-07-28T12:00:00Z",
  };
}

describe("PopoverPanel authoritative daemon phases", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.status = status({ kind: "monitoring" });
    fixture.environments[0]!.accounts[1]!.usageStatus = "ok";
  });

  it("does not infer warning or exhaustion from high usage", () => {
    const { rerender } = render(<PopoverPanel />);
    expect(screen.queryByText(/switching in/i)).not.toBeInTheDocument();
    expect(screen.queryByText(/all accounts at their limit/i)).not.toBeInTheDocument();

    mocks.status = status({ kind: "disabled" }, 2);
    rerender(<PopoverPanel />);
    expect(screen.getByText(/auto-switch off/i)).toBeInTheDocument();
    expect(screen.queryByText(/switching in/i)).not.toBeInTheDocument();
  });

  it("renders the backend warning target and deadline without switching at zero", () => {
    mocks.status = status({
      kind: "warning",
      from: 1,
      to: 2,
      deadline: new Date(Date.now() - 1000).toISOString(),
    });
    render(<PopoverPanel />);

    expect(screen.getByText(/switching now/i)).toBeInTheDocument();
    expect(screen.getByText("next")).toBeInTheDocument();
    expect(mocks.switchAccount).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Hold 1h" }));
    expect(mocks.snooze).toHaveBeenCalledWith(3600);
  });

  it("renders persisted pause and resume, plus cooldown", () => {
    mocks.status = status({ kind: "paused", until: "2026-07-28T13:00:00Z" });
    const { rerender } = render(<PopoverPanel />);
    expect(screen.getByText(/paused until/i)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Resume" }));
    expect(mocks.resume).toHaveBeenCalledOnce();

    mocks.status = status({ kind: "cooldown", until: "2026-07-28T12:05:00Z" }, 2);
    rerender(<PopoverPanel />);
    expect(screen.getByText(/cooldown until/i)).toBeInTheDocument();
  });

  it("renders backend-only exhausted, degraded, and recovery-required states", () => {
    mocks.status = status({ kind: "exhausted", earliestReset: null });
    const { rerender } = render(<PopoverPanel />);
    expect(screen.getByText(/all accounts at their limit/i)).toBeInTheDocument();
    expect(screen.queryByText(/notify me at reset/i)).not.toBeInTheDocument();

    mocks.status = status({ kind: "degraded", reason: "usageUnknown" }, 2);
    rerender(<PopoverPanel />);
    expect(screen.getByText(/usage is currently unknown/i)).toBeInTheDocument();

    mocks.status = status({ kind: "recoveryRequired", detail: "journal requires repair" }, 3);
    rerender(<PopoverPanel />);
    expect(screen.getByText(/recovery required/i)).toBeInTheDocument();
    expect(screen.getByText(/journal requires repair/i)).toBeInTheDocument();
  });

  it("keeps direct account switching manual-only and never calls preview_target", () => {
    render(<PopoverPanel />);
    fireEvent.click(screen.getByRole("button", { name: /Next/i }));
    expect(mocks.switchAccount).toHaveBeenCalledWith(2);
  });

  it("labels a dead credential distinctly and disables manual activation", () => {
    fixture.environments[0]!.accounts[1]!.usageStatus = "reloginrequired";
    render(<PopoverPanel />);

    const account = screen.getByRole("button", { name: /Next.*Re-login required/i });
    expect(account).toBeDisabled();
    expect(screen.getByText("Re-login required")).toBeInTheDocument();
    expect(screen.queryByText(/expired/i)).not.toBeInTheDocument();
    fireEvent.click(account);
    expect(mocks.switchAccount).not.toHaveBeenCalled();
  });
});
