import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { Sample, Snapshot } from "@/types";

const mocks = vi.hoisted(() => ({
  historySamples: vi.fn(),
}));

vi.mock("@/lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/lib/api")>()),
  historySamples: mocks.historySamples,
}));

import HomeScreen, { type HomeScreenProps } from "@/components/home/HomeScreen";

const T0 = Date.parse("2026-07-20T09:00:00Z");

function sample(minutes: number, binding: number): Sample {
  return {
    accountKey: "k",
    timestamp: new Date(T0 - minutes * 60_000).toISOString(),
    fiveHourPct: binding,
    sevenDayPct: binding,
    bindingPct: binding,
    scoped: [],
  };
}

const snapshot: Snapshot = {
  schemaVersion: 1,
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
          email: "one@example.com",
          alias: "Alpha",
          active: true,
          usageStatus: "ok",
          usageAgeSeconds: 10,
          usage: {
            fiveHour: { pct: 62, resetsAt: new Date(T0 + 4 * 3_600_000 + 21 * 60_000).toISOString(), countdown: "9h 9m", clock: "20:39" },
            sevenDay: { pct: 44 },
          },
        },
        {
          number: 2,
          email: "two@example.com",
          alias: "Beta",
          active: false,
          usageStatus: "ok",
          usageAgeSeconds: 10,
          usage: { fiveHour: { pct: 8 }, sevenDay: { pct: 12 } },
        },
        {
          number: 3,
          email: "three@example.com",
          alias: "Gamma",
          active: false,
          usageStatus: "disabled",
          usageAgeSeconds: 10,
          usage: { fiveHour: { pct: 0 }, sevenDay: { pct: 3 } },
        },
      ],
    },
  ],
};

// v0.4 accounts have their own Claude Code folder.
for (const account of snapshot.environments[0]!.accounts) {
  account.profile = { isDefault: account.number === 1, launcher: account.alias?.toLowerCase(), state: "ready" };
}

function props(overrides: Partial<HomeScreenProps> = {}): HomeScreenProps {
  return {
    snapshot,
    settings: null,
    now: T0,
    degraded: false,
    loginPresent: true,
    drawer: null,
    onDrawerChange: vi.fn(),
    onSwitch: vi.fn(),
    pendingAccount: null,
    switchError: null,
    onAddAccount: vi.fn(),
    pendingAddAccount: false,
    addAccountError: null,
    onInteractiveLogin: vi.fn(),
    pendingInteractiveLogin: false,
    interactiveLoginError: null,
    onRelogin: vi.fn(),
    pendingReloginAccount: null,
    reloginError: null,
    onSetEnabled: vi.fn(),
    pendingEnableAccount: null,
    enableError: null,
    onRename: vi.fn().mockResolvedValue(undefined),
    onRemove: vi.fn().mockResolvedValue(undefined),
    onReorder: vi.fn(),
    onWake: vi.fn(),
    pendingWake: null,
    wakeError: null,
    onViewHistory: vi.fn(),
    mutationInFlight: false,
    ...overrides,
  };
}

beforeEach(() => {
  vi.clearAllMocks();
  vi.useFakeTimers({ shouldAdvanceTime: true });
  vi.setSystemTime(T0);
  mocks.historySamples.mockResolvedValue({ data: [sample(120, 20), sample(60, 40), sample(5, 62)], live: true });
});

afterEach(() => vi.useRealTimers());

describe("Home coverage", () => {
  it("projects a clock time once the account in use is burning, and labels it an estimate", async () => {
    render(<HomeScreen {...props()} />);
    await waitFor(() => expect(screen.getByTestId("coverage-value").textContent).toMatch(/\d/));
    expect(screen.getByText("estimate")).toBeInTheDocument();
  });

  it("says idle, not a runway, when nothing is burning", async () => {
    mocks.historySamples.mockResolvedValue({ data: [sample(60, 40), sample(5, 40)], live: true });
    render(<HomeScreen {...props()} />);
    await waitFor(() => expect(screen.getByTestId("coverage-value")).toHaveTextContent("idle"));
    expect(screen.getByText(/nothing is burning right now/)).toBeInTheDocument();
  });

  it("leaves a held-out account out of the projection and says so on its lane", async () => {
    render(<HomeScreen {...props()} />);
    await waitFor(() => expect(screen.getByText(/from 2 of 3 accounts/)).toBeInTheDocument());
    expect(screen.getByRole("img", { name: "Gamma: held out" })).toBeInTheDocument();
  });

  describe("before the first reading", () => {
    const blank: Snapshot = {
      ...snapshot,
      environments: [
        { ...snapshot.environments[0]!, accounts: snapshot.environments[0]!.accounts.map((a) => ({ ...a, usage: undefined })) },
      ],
    };

    it("never reports zero for usage it could not read", async () => {
      render(<HomeScreen {...props({ snapshot: blank })} />);
      expect(await screen.findByText("waiting for the first reading")).toBeInTheDocument();
      expect(screen.getByTestId("coverage-value")).toHaveTextContent("unknown");
      expect(screen.getByTestId("coverage-value")).not.toHaveTextContent("0%");
    });

    it("but does say so when a refresh actually failed", async () => {
      render(<HomeScreen {...props({ snapshot: blank, degraded: true })} />);
      expect(await screen.findByText(/no usage could be read/)).toBeInTheDocument();
    });
  });
});

describe("Home accounts table", () => {
  it("counts down from the limiting window's instant, not the backend's cached strings", () => {
    render(<HomeScreen {...props()} />);
    expect(screen.getByRole("columnheader", { name: "Resets in" })).toBeInTheDocument();
    expect(screen.getByText("4h 21m")).toBeInTheDocument();
    expect(screen.queryByText("20:39")).not.toBeInTheDocument();
    expect(screen.queryByText("9h 9m")).not.toBeInTheDocument();
  });

  it("marks the account auto-switch would pick", () => {
    render(<HomeScreen {...props()} />);
    const beta = screen.getByRole("button", { name: "Beta — details" });
    expect(within(beta).getByText("best next")).toBeInTheDocument();
  });

  it("returns a held-out account to auto-switch without opening its drawer", () => {
    const p = props();
    render(<HomeScreen {...p} />);
    fireEvent.click(screen.getByRole("switch", { name: "Gamma available to auto-switch" }));
    expect(p.onSetEnabled).toHaveBeenCalledWith(3, true);
    expect(p.onDrawerChange).not.toHaveBeenCalled();
  });

  it("never offers to hold out the account in use", () => {
    render(<HomeScreen {...props()} />);
    expect(screen.getByRole("switch", { name: "Alpha available to auto-switch" })).toHaveAttribute("aria-disabled", "true");
  });

  it("switches from a row without opening the drawer", () => {
    const p = props();
    render(<HomeScreen {...p} />);
    fireEvent.click(within(screen.getByRole("button", { name: "Beta — details" })).getByRole("button", { name: "Use" }));
    expect(p.onSwitch).toHaveBeenCalledWith(2);
    expect(p.onDrawerChange).not.toHaveBeenCalled();
  });

  it("offers Move, never Switch, for an account still held the v0.3 way", () => {
    const legacy: Snapshot = JSON.parse(JSON.stringify(snapshot));
    delete legacy.environments[0]!.accounts[1]!.profile;
    const p = props({ snapshot: legacy });
    render(<HomeScreen {...p} />);
    const row = screen.getByRole("button", { name: "Beta — details" });
    expect(within(row).getByText("sign in to move")).toBeInTheDocument();
    expect(within(row).queryByRole("button", { name: "Use" })).not.toBeInTheDocument();
    fireEvent.click(within(row).getByRole("button", { name: "Move" }));
    expect(p.onRelogin).toHaveBeenCalledWith(2);
    expect(p.onSwitch).not.toHaveBeenCalled();
  });

  it("offers Re-login instead of Switch for a rejected login, and never says expired", () => {
    const dead: Snapshot = JSON.parse(JSON.stringify(snapshot));
    dead.environments[0]!.accounts[1]!.usageStatus = "reloginrequired";
    const p = props({ snapshot: dead });
    render(<HomeScreen {...p} />);
    const row = screen.getByRole("button", { name: "Beta — details" });
    expect(within(row).getByText("Re-login required")).toBeInTheDocument();
    expect(within(row).queryByRole("button", { name: "Switch" })).not.toBeInTheDocument();
    fireEvent.click(within(row).getByRole("button", { name: "Re-login" }));
    expect(p.onRelogin).toHaveBeenCalledWith(2);
    expect(screen.queryByText(/expired/i)).not.toBeInTheDocument();
  });

  it("moves an account down by sending the full new order", () => {
    const p = props();
    render(<HomeScreen {...p} />);
    fireEvent.click(screen.getByRole("button", { name: "More actions for Alpha" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Move down" }));
    expect(p.onReorder).toHaveBeenCalledWith([2, 1, 3]);
  });

  it("lets a profile account be removed even while new sessions use it", () => {
    render(<HomeScreen {...props()} />);
    fireEvent.click(screen.getByRole("button", { name: "More actions for Alpha" }));
    expect(screen.getByRole("menuitem", { name: "Remove…" })).toBeEnabled();
  });

  it("does not offer to remove a v0.3 account that is live in Claude Code", () => {
    const legacy: Snapshot = JSON.parse(JSON.stringify(snapshot));
    delete legacy.environments[0]!.accounts[0]!.profile;
    render(<HomeScreen {...props({ snapshot: legacy })} />);
    fireEvent.click(screen.getByRole("button", { name: "More actions for Alpha" }));
    expect(screen.getByRole("menuitem", { name: "Remove…" })).toBeDisabled();
  });
});

describe("Home account drawer", () => {
  it("renames inline", async () => {
    const p = props({ drawer: { accountNumber: 2, intent: "rename" } });
    render(<HomeScreen {...p} />);
    const input = screen.getByRole("textbox", { name: "Account name" });
    fireEvent.change(input, { target: { value: "  Personal  " } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() => expect(p.onRename).toHaveBeenCalledWith(2, "Personal"));
  });

  it("asks before removing, with the safe choice focused", async () => {
    const p = props({ drawer: { accountNumber: 2, intent: "view" } });
    render(<HomeScreen {...p} />);
    fireEvent.click(screen.getByRole("button", { name: "Remove…" }));
    expect(p.onRemove).not.toHaveBeenCalled();
    expect(screen.getByRole("button", { name: "Keep it" })).toHaveFocus();
    fireEvent.click(screen.getByRole("button", { name: "Remove account" }));
    await waitFor(() => expect(p.onRemove).toHaveBeenCalledWith(2));
  });
});
