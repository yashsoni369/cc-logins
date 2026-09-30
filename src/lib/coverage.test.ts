import { describe, expect, it } from "vitest";

import { coveragePlan, moveInOrder, predictTarget, projectLimit, weeklyPaceMark } from "@/lib/coverage";
import type { Account, Sample } from "@/types";

const NOW = Date.parse("2026-07-20T12:00:00Z");
const H = 3_600_000;

function account(number: number, patch: Partial<Account> = {}): Account {
  return {
    number,
    email: `a${number}@example.com`,
    active: false,
    usageStatus: "ok",
    usage: { fiveHour: { pct: 10 }, sevenDay: { pct: 10 } },
    ...patch,
  };
}

function samples(points: Array<[hoursAgo: number, pct: number]>): Sample[] {
  return points.map(([hoursAgo, pct]) => ({
    accountKey: "k",
    timestamp: new Date(NOW - hoursAgo * H).toISOString(),
    fiveHourPct: pct,
    sevenDayPct: null,
    bindingPct: pct,
    scoped: [],
  }));
}

describe("predictTarget (mirrors switcher::pick_target)", () => {
  const fleet = [
    account(1, { active: true, usage: { fiveHour: { pct: 90 } } }),
    account(2, { usage: { fiveHour: { pct: 40 }, sevenDay: { pct: 50, resetsAt: new Date(NOW + 48 * H).toISOString() } } }),
    account(3, { usage: { fiveHour: { pct: 10 }, sevenDay: { pct: 20, resetsAt: new Date(NOW + 96 * H).toISOString() } } }),
  ];

  it("most-headroom picks the emptiest candidate", () => {
    expect(predictTarget(fleet, "most-headroom", NOW)?.number).toBe(3);
  });

  it("next-available picks the first candidate in rotation order", () => {
    expect(predictTarget(fleet, "next-available", NOW)?.number).toBe(2);
  });

  it("consume-first picks the soonest weekly reset", () => {
    expect(predictTarget(fleet, "consume-first", NOW)?.number).toBe(2);
  });

  it("never picks the active, held-out, unreadable or exhausted accounts", () => {
    const blocked = [
      account(1, { active: true }),
      account(2, { usageStatus: "disabled" }),
      account(3, { usageStatus: "stale" }),
      account(4, { usage: undefined }),
      account(5, { usage: { fiveHour: { pct: 100 } } }),
    ];
    for (const strategy of ["most-headroom", "next-available", "consume-first"] as const) {
      expect(predictTarget(blocked, strategy, NOW)).toBeNull();
    }
  });

  it("keeps the earlier slot on a headroom tie, like the backend", () => {
    const tie = [account(1, { active: true }), account(2), account(3)];
    expect(predictTarget(tie, "most-headroom", NOW)?.number).toBe(2);
  });
});

describe("moveInOrder", () => {
  const list = [account(1), account(2), account(3)];
  it("swaps with the neighbour", () => {
    expect(moveInOrder(list, 2, -1)).toEqual([2, 1, 3]);
    expect(moveInOrder(list, 2, 1)).toEqual([1, 3, 2]);
  });
  it("leaves the order alone at either end", () => {
    expect(moveInOrder(list, 1, -1)).toEqual([1, 2, 3]);
    expect(moveInOrder(list, 3, 1)).toEqual([1, 2, 3]);
  });
});

describe("projectLimit", () => {
  const active = account(1, { active: true, usage: { fiveHour: { pct: 60, resetsAt: new Date(NOW + 3 * H).toISOString() } } });

  it("projects when the limit lands at the recent burn rate", () => {
    // 40% -> 60% over two hours = 10%/h, so 100% is four hours away.
    const p = projectLimit(active, samples([[2, 40], [0, 60]]), NOW);
    expect(p.at).toBe(NOW + 4 * H);
    expect(p.beforeReset).toBe(false); // the window resets after 3h
  });

  it("says unknown, never 'forever', when usage is flat", () => {
    expect(projectLimit(active, samples([[2, 60], [0, 60]]), NOW).at).toBeNull();
  });

  it("does not project an enterprise spend cap", () => {
    const ent = account(1, { active: true, usage: { spend: { pct: 50, used: 100, limit: 200, currency: "USD" } } });
    expect(projectLimit(ent, samples([[2, 40], [0, 50]]), NOW).at).toBeNull();
  });
});

describe("coveragePlan", () => {
  const base = {
    now: NOW,
    horizonHours: 12,
    threshold: 90,
    strategy: "most-headroom" as const,
  };
  const fleet = [
    account(1, { active: true, usage: { fiveHour: { pct: 60, resetsAt: new Date(NOW + 10 * H).toISOString() } } }),
    account(2),
    account(3, { usageStatus: "disabled" }),
    account(4, { usage: undefined }),
  ];
  const burn = samples([[2, 40], [0, 60]]); // 10%/h

  it("hands off at the threshold only when auto-switch is on", () => {
    const on = coveragePlan({ ...base, accounts: fleet, activeSamples: burn, autoSwitch: true });
    expect(on.handoff).toEqual({ from: 1, to: 2, at: 3 });
    const off = coveragePlan({ ...base, accounts: fleet, activeSamples: burn, autoSwitch: false });
    expect(off.handoff).toBeNull();
    expect(off.lanes[0]!.limit).toBe(4);
  });

  it("draws unknown usage as unknown and held-out as unavailable", () => {
    const plan = coveragePlan({ ...base, accounts: fleet, activeSamples: burn, autoSwitch: false });
    expect(plan.lanes[3]!.unknown).toBe(true);
    expect(plan.lanes[3]!.segments).toEqual([]);
    expect(plan.lanes[2]!.unavailable).toBe(true);
  });
});

describe("weeklyPaceMark", () => {
  it("prefers the server's expectation", () => {
    expect(weeklyPaceMark({ sevenDay: { pct: 30, expectedPct: 42 } }, NOW)).toBe(42);
  });
  it("otherwise derives it from the time elapsed in the week", () => {
    const resetsAt = new Date(NOW + 3.5 * 24 * H).toISOString();
    expect(weeklyPaceMark({ sevenDay: { pct: 30, resetsAt } }, NOW)).toBeCloseTo(50);
  });
  it("is null without a weekly window", () => {
    expect(weeklyPaceMark({ fiveHour: { pct: 30 } }, NOW)).toBeNull();
  });
});
