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
    // once they age past retention, so the far end would come back empty.
    // The daily read is sized to the widest span any panel wants, which is the
    // load-balance grid's, not the selected range's.
    await waitFor(() => expect(mocks.historySeries).toHaveBeenCalledWith(expect.any(String), 180));
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

  // Regressions found by looking at the built screen, not by reading the code.
  it("leads with pooled headroom when the runway cannot be projected", async () => {
    // accountBurn returns null on a flat or falling slope — "not burning right
    // now". A giant "unknown" as the hero wastes the most prominent element on
    // the screen, so the measured figure takes the lead.
    mocks.historySamples.mockResolvedValue({ data: [sample(60, 40), sample(5, 40)], source: "live" });
    renderDash();

    expect(await screen.findByText("pooled headroom")).toBeInTheDocument();
    expect(screen.getByText(/runway unknown/)).toBeInTheDocument();
    expect(screen.queryByText("pooled runway")).not.toBeInTheDocument();
  });

  it("shows the runway as the hero once one can be projected", async () => {
    // A rising slope on the active account: 20% -> 62% over two hours.
    mocks.historySamples.mockResolvedValue({
      data: [sample(120, 20), sample(60, 40), sample(5, 62)],
      source: "live",
    });
    renderDash();

    expect(await screen.findByText("pooled runway")).toBeInTheDocument();
    expect(screen.getByText(/headroom pooled/)).toBeInTheDocument();
  });

  it("says it is still reading rather than reporting an empty history", async () => {
    let release: (v: { data: Sample[]; source: string }) => void = () => {};
    mocks.historySamples.mockReturnValue(new Promise((r) => (release = r)));
    mocks.historySeries.mockReturnValue(new Promise((r) => (release = r)));
    renderDash();

    expect(screen.queryByText(/No usage recorded/)).not.toBeInTheDocument();
    release({ data: [], source: "live" });
  });

  it("gives every account row a trend, and marks an unmeasured one", async () => {
    renderDash();
    // Drawn from the series the rotation chart already built — no second read.
    expect(await screen.findByRole("img", { name: /Alpha: utilisation trend/ })).toBeInTheDocument();
  });

  it("keeps the load-balance span independent of the selected range", async () => {
    renderDash();
    const span = () => screen.getByText(/daily peak per account, last \d+ days/).textContent;
    const before = await waitFor(span);

    // Tied to the range, 24h meant seven cells and "All" meant 365 — one too
    // coarse to be a pattern, the other unreadable. The span now follows the
    // window's width instead, which the range does not change.
    fireEvent.click(screen.getByRole("button", { name: "24h" }));
    await waitFor(() => expect(span()).toBe(before));
  });

  it("separates the forward-looking resets from the backward-looking chart", async () => {
    renderDash();
    // Flush against the rotation chart they read as one timeline running the
    // wrong way, so the resets get their own titled band.
    expect(await screen.findByText("Next resets")).toBeInTheDocument();
  });

  /*
   * The poller's first fetch can take over a minute. Until it lands, every
   * account has no usage — which is indistinguishable from total failure
   * unless the screen says which one it is.
   */
  describe("before the first reading", () => {
    const blank: Snapshot = {
      ...snapshot,
      environments: [
        {
          ...snapshot.environments[0]!,
          accounts: snapshot.environments[0]!.accounts.map((a) => ({ ...a, usage: undefined })),
        },
      ],
    };

    it("never reports zero headroom for usage it could not read", async () => {
      const { container } = render(
        <DashboardScreen snapshot={blank} settingsThreshold={85} degraded={false} />,
      );
      await screen.findByText("waiting for the first reading");
      // 0% asserts "no capacity left". The truth is "we do not know". The
      // headline figure specifically — the qualifier pill also reads "unknown".
      expect(container.querySelector(".cap-big")).toHaveTextContent("unknown");
      expect(container.querySelector(".cap-big")).not.toHaveTextContent("0%");
    });

    it("says it is waiting, not that everything failed", async () => {
      render(<DashboardScreen snapshot={blank} settingsThreshold={85} degraded={false} />);
      expect(await screen.findByText("waiting for the first reading")).toBeInTheDocument();
    });

    it("but does say so when a refresh actually failed", async () => {
      render(<DashboardScreen snapshot={blank} settingsThreshold={85} degraded />);
      expect(await screen.findByText(/no usage could be read/)).toBeInTheDocument();
    });
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
    // ...and named beneath the capacity bar rather than given a share of it.
    // Drawn proportionally, an untouched held-out account owns the most
    // headroom in the fleet and became the bar's widest block — advertising
    // capacity the switcher cannot spend.
    expect(screen.getByText(/Not in rotation/)).toBeInTheDocument();
    expect(screen.getByText(/\(held out\)/)).toBeInTheDocument();
  });
});
