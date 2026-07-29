import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { DayStat, Sample, Snapshot } from "@/types";

const mocks = vi.hoisted(() => ({
  historySamples: vi.fn(),
  historySeries: vi.fn(),
}));

vi.mock("@/lib/api", async (importOriginal) => ({
  ...(await importOriginal<typeof import("@/lib/api")>()),
  historySamples: mocks.historySamples,
  historySeries: mocks.historySeries,
}));

import DashboardScreen from "@/components/DashboardScreen";

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

function day(d: string, max: number): DayStat {
  return { day: d, minPct: 0, maxPct: max, avgPct: max / 2, sampleCount: 10 };
}

const snapshot: Snapshot = {
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
          email: "one@example.com",
          alias: "Alpha",
          active: true,
          usageStatus: "ok",
          usage: {
            fiveHour: { pct: 62, resetsAt: new Date(T0 + 2 * 3_600_000).toISOString() },
            sevenDay: { pct: 44 },
            scoped: [{ name: "Opus 5", pct: 70, resetsAt: new Date(T0 + 3 * 3_600_000).toISOString() }],
          },
        },
        {
          number: 2,
          email: "two@example.com",
          alias: "Beta",
          active: false,
          usageStatus: "ok",
          usage: { fiveHour: { pct: 8 }, sevenDay: { pct: 12 } },
        },
      ],
    },
  ],
};

function renderDash(overrides: Partial<Snapshot> = {}) {
  return render(
    <DashboardScreen snapshot={{ ...snapshot, ...overrides }} settingsThreshold={85} degraded={false} />,
  );
}

beforeEach(() => {
  vi.clearAllMocks();
  vi.useFakeTimers({ shouldAdvanceTime: true });
  vi.setSystemTime(T0);
  mocks.historySamples.mockResolvedValue({ data: [sample(120, 20), sample(60, 55), sample(5, 62)], source: "live" });
  mocks.historySeries.mockResolvedValue({ data: [day("2026-07-19", 90), day("2026-07-20", 62)], source: "live" });
});

afterEach(() => {
  vi.useRealTimers();
});

describe("DashboardScreen", () => {
  it("keeps the fleet on screen when a row is opened", async () => {
    renderDash();
    const row = await screen.findByRole("button", { name: /Alpha/ });
    expect(row).toHaveAttribute("aria-expanded", "false");

    fireEvent.click(row);

    expect(row).toHaveAttribute("aria-expanded", "true");
    // The whole point of the disclosure: the other account is still there to
    // compare against, which the old full-screen swap destroyed.
    expect(screen.getByRole("button", { name: /Beta/ })).toBeInTheDocument();
  });

  it("collapses again on a second click", async () => {
    renderDash();
    const row = await screen.findByRole("button", { name: /Alpha/ });
    fireEvent.click(row);
    fireEvent.click(row);
    expect(row).toHaveAttribute("aria-expanded", "false");
  });

  it("opens two accounts at once", async () => {
    renderDash();
    fireEvent.click(await screen.findByRole("button", { name: /Alpha/ }));
    fireEvent.click(await screen.findByRole("button", { name: /Beta/ }));
    expect(screen.getByRole("button", { name: /Alpha/ })).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByRole("button", { name: /Beta/ })).toHaveAttribute("aria-expanded", "true");
  });

  it("fetches an account's history once, however often the row is toggled", async () => {
    renderDash();
    const row = await screen.findByRole("button", { name: /Alpha/ });
    await waitFor(() => expect(mocks.historySamples).toHaveBeenCalled());
    const before = mocks.historySamples.mock.calls.length;

    fireEvent.click(row);
    await waitFor(() => expect(mocks.historySamples.mock.calls.length).toBeGreaterThan(before));
    const afterOpen = mocks.historySamples.mock.calls.length;

    fireEvent.click(row);
    fireEvent.click(row);
    fireEvent.click(row);
    // Re-opening reads the cache; only the first open may fetch.
    await waitFor(() => expect(mocks.historySamples.mock.calls.length).toBe(afterOpen));
  });

  it("switches tabs inside an open row", async () => {
    renderDash();
    fireEvent.click(await screen.findByRole("button", { name: /Alpha/ }));

    const models = await screen.findByRole("tab", { name: "Models" });
    fireEvent.click(models);
    expect(models).toHaveAttribute("aria-selected", "true");
    // Per-model windows come from live usage, which is the only source that
    // carries a reset.
    expect(await screen.findByText("Opus 5")).toBeInTheDocument();
  });

  it("offers every other account to compare against, and never itself", async () => {
    renderDash();
    fireEvent.click(await screen.findByRole("button", { name: /Alpha/ }));

    const select = await screen.findByRole("combobox", { name: /Compare Alpha against/i });
    const options = within(select).getAllByRole("option").map((o) => o.textContent);
    expect(options).toEqual(["compare: none", "compare: Beta"]);
  });

  it("re-reads history when the range changes", async () => {
    renderDash();
    await waitFor(() => expect(mocks.historySeries).toHaveBeenCalled());
    mocks.historySamples.mockClear();
    mocks.historySeries.mockClear();

    fireEvent.click(screen.getByRole("button", { name: "30 days" }));

    // A 30-day range must read rollups, never samples — prune deletes samples
    // once they age past retention.
    await waitFor(() => expect(mocks.historySeries).toHaveBeenCalledWith(expect.any(String), 30));
    expect(mocks.historySamples).not.toHaveBeenCalledWith(expect.any(String), 720);
  });

  it("survives an account with no recorded history", async () => {
    mocks.historySamples.mockResolvedValue({ data: [], source: "live" });
    mocks.historySeries.mockResolvedValue({ data: [], source: "live" });
    renderDash();

    fireEvent.click(await screen.findByRole("button", { name: /Alpha/ }));
    expect(await screen.findByText(/No history yet for this account/)).toBeInTheDocument();
  });

  it("survives a history read that throws, rather than blanking the screen", async () => {
    mocks.historySamples.mockRejectedValue(new Error("store locked"));
    mocks.historySeries.mockRejectedValue(new Error("store locked"));
    renderDash();

    // The fleet is still listed; only the charts are empty.
    expect(await screen.findByRole("button", { name: /Alpha/ })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: /Beta/ })).toBeInTheDocument();
  });

  it("tells the truth about an empty fleet instead of drawing empty axes", () => {
    renderDash({ environments: [] });
    expect(screen.getByText("No accounts yet")).toBeInTheDocument();
    expect(screen.queryByRole("group", { name: "Time range" })).not.toBeInTheDocument();
  });

  it("names a held-out account as held out and leaves it out of pooled capacity", async () => {
    const held: Snapshot = JSON.parse(JSON.stringify(snapshot));
    held.environments[0]!.accounts[1]!.usageStatus = "disabled";
    render(<DashboardScreen snapshot={held} settingsThreshold={85} degraded={false} />);

    // Named on its own row...
    expect(await screen.findByRole("button", { name: /Beta.*held out/ })).toBeInTheDocument();
    // ...and marked unreachable in pooled capacity, which is the figure that
    // would otherwise promise headroom the switcher cannot spend.
    expect(screen.getByTitle(/Beta.*held out of rotation/)).toBeInTheDocument();
  });
});
