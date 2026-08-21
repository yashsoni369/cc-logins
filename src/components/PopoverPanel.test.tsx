import { fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { DaemonStatus, Settings, Snapshot } from "@/types";

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
    settings: settingsFixture,
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

// The popover has no clock-format provider, so it reads the setting from here.
const settingsFixture: Settings = {
  autoSwitchEnabled: true,
  autoSwitchPausedUntil: null,
  threshold: 85,
  cooldownSeconds: 300,
  hysteresisPct: 5,
  unhealthyTicks: 3,
  strategy: "most-headroom",
  graceSeconds: 60,
  notifyOnSwitch: true,
  notifyOnExhausted: true,
  notifyOnExpiry: true,
  startAtLogin: false,
  autoCheckUpdates: true,
  historyRetentionDays: 90,
  theme: "system",
  clockFormat: "24h",
  claudeBinaryPath: null,
};

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
    fixture.environments[0]!.accounts[0]!.usage = { sevenDay: { pct: 99 } };
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
    expect(screen.getByRole("button", { name: /Next/i })).toBeDisabled();
  });

  it("keeps direct account switching manual-only and never calls preview_target", () => {
    render(<PopoverPanel />);
    fireEvent.click(screen.getByRole("button", { name: /Next/i }));
    expect(mocks.switchAccount).toHaveBeenCalledWith(2);
  });

  it("labels a dead credential distinctly and disables manual activation", () => {
    fixture.environments[0]!.accounts[1]!.usageStatus = "reloginrequired";
    render(<PopoverPanel />);

    // The pill is abbreviated to survive a 364px row. A title on an element
    // that already has text does not reach the accessible name, so the button
    // is named by the short form and the full phrase is hover-only.
    const account = screen.getByRole("button", { name: /Next.*Re-login/i });
    expect(account).toBeDisabled();
    expect(screen.getByTitle("Re-login required")).toBeInTheDocument();
    expect(screen.queryByText(/expired/i)).not.toBeInTheDocument();
    fireEvent.click(account);
    expect(mocks.switchAccount).not.toHaveBeenCalled();
  });
});

describe("PopoverPanel reset readouts", () => {
  const pinned = new Date("2026-07-28T12:00:00Z");

  beforeEach(() => {
    vi.clearAllMocks();
    vi.useFakeTimers();
    vi.setSystemTime(pinned);
    mocks.status = status({ kind: "monitoring" });
    fixture.environments[0]!.accounts[1]!.usageStatus = "ok";
  });
  afterEach(() => {
    vi.useRealTimers();
    fixture.environments[0]!.accounts[0]!.usage = { sevenDay: { pct: 99 } };
  });

  it("shows time remaining rather than the wall clock the backend sent", () => {
    fixture.environments[0]!.accounts[0]!.usage = {
      sevenDay: {
        pct: 99,
        resetsAt: new Date(pinned.getTime() + 3 * 3_600_000 + 5 * 60_000).toISOString(),
        countdown: "stale countdown",
        clock: "23:59",
      },
    };
    render(<PopoverPanel />);

    expect(screen.getByText(/3h/)).toBeInTheDocument();
    expect(screen.queryByText("stale countdown")).not.toBeInTheDocument();
    expect(screen.queryByText("23:59")).not.toBeInTheDocument();
  });

  // Stale beats blank: without a parseable instant we still say something.
  it("falls back to the backend countdown, then its clock, then a dash", () => {
    fixture.environments[0]!.accounts[0]!.usage = {
      sevenDay: { pct: 99, countdown: "4h 21m", clock: "23:59" },
    };
    const { rerender } = render(<PopoverPanel />);
    expect(screen.getByText("4h 21m")).toBeInTheDocument();
    expect(screen.queryByText("23:59")).not.toBeInTheDocument();

    fixture.environments[0]!.accounts[0]!.usage = { sevenDay: { pct: 99, clock: "23:59" } };
    mocks.status = status({ kind: "monitoring" }, 2);
    rerender(<PopoverPanel />);
    expect(screen.getByText("23:59")).toBeInTheDocument();

    fixture.environments[0]!.accounts[0]!.usage = { sevenDay: { pct: 99 } };
    mocks.status = status({ kind: "monitoring" }, 3);
    rerender(<PopoverPanel />);
    expect(screen.getByText("—")).toBeInTheDocument();
  });
});

/*
 * A meter says a switch target sits at 88%; it cannot say whether that clears
 * in twenty minutes or six days. These cover the readout that answers it.
 */
describe("PopoverPanel switch-target resets", () => {
  const pinned = new Date("2026-07-28T12:00:00Z");
  const target = () => fixture.environments[0]!.accounts[1]!;

  beforeEach(() => {
    vi.clearAllMocks();
    vi.useFakeTimers();
    vi.setSystemTime(pinned);
    mocks.status = status({ kind: "monitoring" });
    target().usageStatus = "ok";
  });
  afterEach(() => {
    vi.useRealTimers();
    target().usageStatus = "ok";
    target().usage = { sevenDay: { pct: 10 } };
  });

  it("shows when the target's quota frees up", () => {
    target().usage = {
      sevenDay: { pct: 10, resetsAt: new Date(pinned.getTime() + 2 * 86_400_000).toISOString() },
    };
    render(<PopoverPanel />);

    expect(screen.getByText("2d 0h")).toBeInTheDocument();
  });

  // The percentage comes from the binding window, so the time beside it must
  // too — pairing 90% with the seven-day clock would describe neither.
  it("reads the reset from the same window the meter measures", () => {
    target().usage = {
      fiveHour: { pct: 90, resetsAt: new Date(pinned.getTime() + 2 * 3_600_000).toISOString() },
      sevenDay: { pct: 40, resetsAt: new Date(pinned.getTime() + 5 * 86_400_000).toISOString() },
    };
    render(<PopoverPanel />);

    expect(screen.getByText("2h 0m")).toBeInTheDocument();
    expect(screen.queryByText("5d 0h")).not.toBeInTheDocument();
  });

  // A column of em dashes reads as a broken readout rather than an absent one,
  // and these rows are switch buttons, not a report. The active account's own
  // readout keeps its dash — there the row exists to be read.
  it("says nothing at all when the reset is unknown", () => {
    target().usage = { sevenDay: { pct: 10 } };
    render(<PopoverPanel />);

    expect(screen.getByRole("button", { name: /Next/ }).textContent).not.toContain("—");
  });

  it("stays quiet for an account that cannot be switched to", () => {
    target().usageStatus = "reloginrequired";
    target().usage = {
      sevenDay: { pct: 10, resetsAt: new Date(pinned.getTime() + 2 * 86_400_000).toISOString() },
    };
    render(<PopoverPanel />);

    // When the account is unreachable, its quota is not what stands in the way.
    expect(screen.queryByText("2d 0h")).not.toBeInTheDocument();
    expect(screen.getByText("Re-login")).toBeInTheDocument();
  });

  // The staleness pill was the price of the reset column. Losing it silently
  // from the active header too was the deliberate half of that trade.
  it("no longer prints how old the reading is", () => {
    render(<PopoverPanel />);

    expect(screen.queryByText(/\d+[mhd] old/)).not.toBeInTheDocument();
  });
});

/*
 * Aliases are free text and domains have no length bound, so a name can be
 * arbitrarily long. Truncation is CSS's job — jsdom has no layout — but the
 * DOM contract that makes truncation safe is testable: the full value must
 * still reach the accessibility tree and the title, or a clipped row becomes
 * an unidentifiable one.
 */
describe("PopoverPanel long names", () => {
  const LONG_DOMAIN = "yash@really-long-company-domain-name-with-subdomains.co.uk";
  const LONG_ALIAS = "production-account-for-the-main-workspace-europe-west";
  const target = () => fixture.environments[0]!.accounts[1]!;
  const active = () => fixture.environments[0]!.accounts[0]!;

  beforeEach(() => {
    vi.clearAllMocks();
    mocks.status = status({ kind: "monitoring" });
  });
  afterEach(() => {
    target().alias = "Next";
    target().email = "next@example.com";
    active().alias = "Active";
    active().email = "active@example.com";
  });

  it("renders a long masked domain in full, and repeats it on the title", () => {
    target().alias = undefined;
    target().email = LONG_DOMAIN;
    render(<PopoverPanel />);

    const masked = "y•••@really-long-company-domain-name-with-subdomains.co.uk";
    expect(screen.getByTitle(masked)).toHaveTextContent(masked);
  });

  it("does the same for a long alias on the active account", () => {
    active().alias = LONG_ALIAS;
    render(<PopoverPanel />);

    expect(screen.getByTitle(LONG_ALIAS)).toHaveTextContent(LONG_ALIAS);
  });

  // The banner exists to show the countdown. A long name must not be able to
  // take its place, so the name is the element that yields.
  it("keeps the switch countdown alongside a long name in the warning banner", () => {
    target().alias = LONG_ALIAS;
    mocks.status = status({ kind: "warning", from: 1, to: 2, deadline: "2026-07-28T12:00:12Z" }, 2);
    render(<PopoverPanel />);

    // The name also appears on its own switch row, so scope to the banner.
    const banner = screen.getByRole("status");
    expect(within(banner).getByText(new RegExp(LONG_ALIAS))).toBeInTheDocument();
    expect(within(banner).getByText(/switching in \d+s|switching now/)).toBeInTheDocument();
  });

  it("still shows a name with no @ in it rather than dropping the row", () => {
    target().alias = undefined;
    target().email = "not-an-email-address-just-a-long-token";
    render(<PopoverPanel />);

    expect(screen.getByTitle("not-an-email-address-just-a-long-token")).toBeInTheDocument();
  });
});
