import { describe, expect, it } from "vitest";

import {
  RANGES,
  buildFleetSeries,
  deriveInsights,
  isRangeKey,
  loadBalanceGrid,
  pooledHeadroom,
  runsOf,
  thin,
  type Pt,
} from "./dashboard";
import type { Account, DayStat, Sample } from "@/types";

const HOUR = 3_600_000;
const T0 = Date.parse("2026-07-20T00:00:00Z");

function account(fields: Partial<Account>): Account {
  return { number: 1, email: "a@example.com", active: false, usageStatus: "ok", ...fields };
}

function sample(minutes: number, binding: number, extra: Partial<Sample> = {}): Sample {
  return {
    accountKey: "k",
    timestamp: new Date(T0 + minutes * 60_000).toISOString(),
    fiveHourPct: binding,
    sevenDayPct: binding,
    bindingPct: binding,
    scoped: [],
    ...extra,
  };
}

function day(d: string, max: number, min = 0, avg = (max + min) / 2, count = 12): DayStat {
  return { day: d, minPct: min, maxPct: max, avgPct: avg, sampleCount: count };
}

const keyFor = (a: Account) => `key-${a.number}`;

// ── ranges ───────────────────────────────────────────────────────────────────

describe("ranges", () => {
  it("uses samples for short ranges and daily rollups for long ones", () => {
    // Long ranges must not ask for samples: prune deletes them once they age
    // past retention, so the far end of the window would silently empty out.
    expect(RANGES["24h"].source).toBe("samples");
    expect(RANGES["7d"].source).toBe("samples");
    expect(RANGES["30d"].source).toBe("daily");
    expect(RANGES.all.source).toBe("daily");
  });

  it("recognises only real range keys", () => {
    expect(isRangeKey("7d")).toBe(true);
    expect(isRangeKey("90d")).toBe(false);
    expect(isRangeKey("")).toBe(false);
  });
});

// ── runsOf ───────────────────────────────────────────────────────────────────

describe("runsOf", () => {
  const time = (s: Sample) => Date.parse(s.timestamp);
  const pick = (s: Sample) => s.bindingPct;

  it("returns nothing for no input", () => {
    expect(runsOf([], time, pick)).toEqual([]);
  });

  it("positions points by time, not by index", () => {
    // Two readings five minutes apart and a third an hour later must not be
    // drawn evenly spaced — that would show a gentle slope where there was a
    // jump nobody watched.
    const runs = runsOf([sample(0, 10), sample(5, 20), sample(65, 30)], time, pick);
    const xs = runs.flat().map((p) => Number(p.x.toFixed(3)));
    expect(xs).toEqual([0, Number((5 / 65).toFixed(3)), 1]);
  });

  it("breaks the line across a gap far longer than the usual cadence", () => {
    const runs = runsOf(
      [sample(0, 10), sample(5, 12), sample(10, 14), sample(600, 40), sample(605, 42)],
      time,
      pick,
    );
    expect(runs).toHaveLength(2);
    expect(runs[0]).toHaveLength(3);
    expect(runs[1]).toHaveLength(2);
  });

  it("does not shred a line when one poll merely ran late", () => {
    // 5-minute cadence with a single 12-minute gap: still one continuous run,
    // because the floor is a quarter hour.
    const runs = runsOf([sample(0, 10), sample(5, 11), sample(17, 12), sample(22, 13)], time, pick);
    expect(runs).toHaveLength(1);
  });

  it("breaks at a null reading rather than drawing through it", () => {
    const runs = runsOf(
      [sample(0, 10), sample(5, 20), sample(10, 30)],
      time,
      (s) => (s.bindingPct === 20 ? null : s.bindingPct),
    );
    expect(runs).toHaveLength(2);
    expect(runs.flat().map((p) => p.v)).toEqual([10, 30]);
  });

  it("sorts defensively, so an out-of-order reading cannot zig-zag backwards", () => {
    const runs = runsOf([sample(10, 30), sample(0, 10), sample(5, 20)], time, pick);
    expect(runs.flat().map((p) => p.v)).toEqual([10, 20, 30]);
  });

  it("clamps values into 0..100 instead of drawing outside the axis", () => {
    const runs = runsOf([sample(0, -20), sample(5, 160)], time, pick);
    expect(runs.flat().map((p) => p.v)).toEqual([0, 100]);
  });

  it("drops unparseable timestamps rather than placing them at the epoch", () => {
    const bad = { ...sample(0, 10), timestamp: "not-a-date" };
    const runs = runsOf([bad, sample(5, 20)], time, pick);
    expect(runs.flat()).toHaveLength(1);
  });

  it("survives a single reading, which is still a measurement", () => {
    const runs = runsOf([sample(0, 42)], time, pick);
    expect(runs).toEqual([[{ x: 0, v: 42 }]]);
  });

  it("returns nothing when every reading is null", () => {
    expect(runsOf([sample(0, 10), sample(5, 20)], time, () => null)).toEqual([]);
  });

  it("treats NaN as unknown, not as zero", () => {
    const runs = runsOf([sample(0, 10), sample(5, Number.NaN), sample(10, 30)], time, pick);
    expect(runs.flat().map((p) => p.v)).toEqual([10, 30]);
  });
});

// ── thin ─────────────────────────────────────────────────────────────────────

describe("thin", () => {
  const run = (values: number[]): Pt[] => values.map((v, i) => ({ x: i / (values.length - 1), v }));

  it("leaves a short run alone", () => {
    const r = run([1, 2, 3]);
    expect(thin(r, 10)).toBe(r);
  });

  it("keeps the peak, which every-nth sampling would drop", () => {
    // The spike is the reason this chart exists: it is what triggers a switch.
    const values = new Array(400).fill(10);
    values[137] = 99;
    const out = thin(run(values), 40);
    expect(out.length).toBeLessThanOrEqual(40);
    expect(Math.max(...out.map((p) => p.v))).toBe(99);
  });

  it("keeps the trough too, so a reset still reads as a reset", () => {
    const values = new Array(400).fill(80);
    values[201] = 2;
    const out = thin(run(values), 40);
    expect(Math.min(...out.map((p) => p.v))).toBe(2);
  });

  it("emits points in time order", () => {
    const values = Array.from({ length: 300 }, (_, i) => (i % 7) * 14);
    const out = thin(run(values), 30);
    const xs = out.map((p) => p.x);
    expect([...xs].sort((a, b) => a - b)).toEqual(xs);
  });

  it("degrades safely at absurd limits", () => {
    expect(thin([], 10)).toEqual([]);
    expect(thin(run([1, 2, 3]), 0)).toHaveLength(1);
    expect(thin(run([1, 2, 3]), 1)).toHaveLength(1);
  });
});

// ── fleet series ─────────────────────────────────────────────────────────────

describe("buildFleetSeries", () => {
  const accounts = [account({ number: 1, active: true }), account({ number: 2 })];
  const spec = RANGES["24h"];

  it("reads samples for a sample-backed range", () => {
    const samples = new Map([["key-1", [sample(0, 10), sample(30, 50)]]]);
    const series = buildFleetSeries(accounts, keyFor, samples, new Map(), spec);
    expect(series[0]?.last).toBe(50);
    expect(series[0]?.peak).toBe(50);
    expect(series[0]?.mean).toBe(30);
  });

  it("reads daily rollups for a daily-backed range", () => {
    const daily = new Map([["key-1", [day("2026-07-18", 80, 0, 40), day("2026-07-19", 90, 0, 60)]]]);
    const series = buildFleetSeries(accounts, keyFor, new Map(), daily, RANGES["30d"]);
    // avgPct is what the line traces; maxPct belongs to the load-balance grid.
    expect(series[0]?.last).toBe(60);
    expect(series[0]?.mean).toBe(50);
  });

  it("still lists an account with no history, so the fleet never loses a row", () => {
    const series = buildFleetSeries(accounts, keyFor, new Map(), new Map(), spec);
    expect(series).toHaveLength(2);
    expect(series[0]?.runs).toEqual([]);
    expect(series[0]?.last).toBeNull();
    expect(series[0]?.mean).toBeNull();
  });

  it("skips an account whose key has not resolved yet rather than keying on undefined", () => {
    const series = buildFleetSeries(accounts, (a) => (a.number === 1 ? "key-1" : undefined), new Map(), new Map(), spec);
    expect(series.map((s) => s.number)).toEqual([1]);
  });

  it("carries active and held-out through, which drive emphasis and exclusion", () => {
    const held = [account({ number: 3, usageStatus: "disabled" })];
    const series = buildFleetSeries(held, keyFor, new Map(), new Map(), spec);
    expect(series[0]?.heldOut).toBe(true);
    expect(series[0]?.active).toBe(false);
  });

  it("thins a large series without losing its peak", () => {
    const many = Array.from({ length: 2000 }, (_, i) => sample(i * 5, i === 900 ? 100 : 20));
    const series = buildFleetSeries([accounts[0] as Account], keyFor, new Map([["key-1", many]]), new Map(), spec, 120);
    const points = series[0]?.runs.flat() ?? [];
    expect(points.length).toBeLessThanOrEqual(130);
    expect(series[0]?.peak).toBe(100);
  });
});

// ── pooled headroom ──────────────────────────────────────────────────────────

describe("pooledHeadroom", () => {
  it("sums the unused quota of usable accounts", () => {
    const result = pooledHeadroom([
      account({ number: 1, usage: { sevenDay: { pct: 40 } } }),
      account({ number: 2, usage: { sevenDay: { pct: 10 } } }),
    ]);
    expect(result.pooled).toBe(150);
    expect(result.usable).toBe(2);
  });

  it("shows a held-out account but does not count it", () => {
    // Counting it would promise headroom the switcher cannot reach.
    const result = pooledHeadroom([
      account({ number: 1, usage: { sevenDay: { pct: 40 } } }),
      account({ number: 2, usageStatus: "disabled", usage: { sevenDay: { pct: 0 } } }),
    ]);
    expect(result.segments).toHaveLength(2);
    expect(result.pooled).toBe(60);
    expect(result.usable).toBe(1);
    expect(result.segments[1]?.excluded).toBe(true);
  });

  it("excludes an account whose usage could not be read, rather than assuming it is free", () => {
    const result = pooledHeadroom([account({ number: 1 })]);
    expect(result.segments[0]?.binding).toBeNull();
    expect(result.segments[0]?.excluded).toBe(true);
    expect(result.pooled).toBe(0);
  });

  it("never reports negative headroom for an over-limit account", () => {
    const result = pooledHeadroom([account({ number: 1, usage: { sevenDay: { pct: 130 } } })]);
    expect(result.segments[0]?.free).toBe(0);
  });

  it("handles an empty fleet", () => {
    expect(pooledHeadroom([])).toMatchObject({ pooled: 0, usable: 0, total: 0, segments: [] });
  });
});

// ── load balance ─────────────────────────────────────────────────────────────

describe("loadBalanceGrid", () => {
  const now = Date.parse("2026-07-20T09:00:00Z");

  it("emits one cell per day, oldest first, ending today", () => {
    const rows = loadBalanceGrid([account({ number: 1 })], keyFor, new Map(), 3, now);
    expect(rows[0]?.cells.map((c) => c.day)).toEqual(["2026-07-18", "2026-07-19", "2026-07-20"]);
  });

  it("leaves an unmeasured day null rather than zero", () => {
    // A gap in recording must never read as a quiet day.
    const daily = new Map([["key-1", [day("2026-07-20", 70)]]]);
    const rows = loadBalanceGrid([account({ number: 1 })], keyFor, daily, 3, now);
    expect(rows[0]?.cells.map((c) => c.peak)).toEqual([null, null, 70]);
  });

  it("reports the day's peak, not its average", () => {
    const daily = new Map([["key-1", [day("2026-07-20", 96, 4, 22)]]]);
    const rows = loadBalanceGrid([account({ number: 1 })], keyFor, daily, 1, now);
    expect(rows[0]?.cells[0]?.peak).toBe(96);
  });

  it("gives an account with an unresolved key a full row of blanks", () => {
    const rows = loadBalanceGrid([account({ number: 1 })], () => undefined, new Map(), 2, now);
    expect(rows[0]?.cells.every((c) => c.peak === null)).toBe(true);
  });

  it("clamps a nonsense span to at least one day", () => {
    expect(loadBalanceGrid([account({ number: 1 })], keyFor, new Map(), 0, now)[0]?.cells).toHaveLength(1);
    expect(loadBalanceGrid([account({ number: 1 })], keyFor, new Map(), -5, now)[0]?.cells).toHaveLength(1);
  });
});

// ── insights ─────────────────────────────────────────────────────────────────

describe("deriveInsights", () => {
  const spec = RANGES["7d"];
  const base = { spec, threshold: 85, now: T0 };

  const series = (values: Array<[number, number]>) =>
    values.map(([number, mean]) => ({
      accountKey: `key-${number}`,
      number,
      name: `acct${number}`,
      runs: [[{ x: 0, v: mean }]],
      last: mean,
      peak: mean,
      mean,
      active: false,
      heldOut: false,
    }));

  it("says nothing at all when there is nothing to say", () => {
    const out = deriveInsights({ ...base, accounts: [], series: [], rows: [] });
    expect(out).toEqual([]);
  });

  it("does not claim concentration from a single account", () => {
    // One account is always 100% of the load; stating it would be noise.
    const out = deriveInsights({ ...base, accounts: [], series: series([[1, 90]]), rows: [] });
    expect(out.find((i) => i.id === "concentration")).toBeUndefined();
  });

  it("does not claim concentration when the split is roughly even", () => {
    const out = deriveInsights({ ...base, accounts: [], series: series([[1, 50], [2, 48]]), rows: [] });
    expect(out.find((i) => i.id === "concentration")).toBeUndefined();
  });

  it("reports concentration once one account clearly dominates", () => {
    const out = deriveInsights({ ...base, accounts: [], series: series([[1, 90], [2, 5], [3, 5]]), rows: [] });
    const found = out.find((i) => i.id === "concentration");
    expect(found?.figure).toBe("90%");
    expect(found?.tone).toBe("danger");
  });

  it("counts saturated days from daily peaks", () => {
    const rows = [
      { number: 1, name: "a", cells: [{ day: "d1", peak: 100, sampleCount: 5 }, { day: "d2", peak: 40, sampleCount: 5 }] },
      { number: 2, name: "b", cells: [{ day: "d1", peak: 99, sampleCount: 5 }] },
    ];
    const found = deriveInsights({ ...base, accounts: [], series: [], rows }).find((i) => i.id === "saturation");
    expect(found?.figure).toBe("2");
  });

  it("ignores unmeasured days when counting saturation", () => {
    const rows = [{ number: 1, name: "a", cells: [{ day: "d1", peak: null, sampleCount: 0 }] }];
    const out = deriveInsights({ ...base, accounts: [], series: [], rows });
    expect(out.find((i) => i.id === "saturation")).toBeUndefined();
  });

  it("flags clustered resets only when three or more land together", () => {
    const at = (mins: number) =>
      account({ number: mins, usage: { fiveHour: { pct: 1, resetsAt: new Date(T0 + mins * 60_000).toISOString() } } });

    const spread = deriveInsights({ ...base, accounts: [at(10), at(200), at(400)], series: [], rows: [] });
    expect(spread.find((i) => i.id === "clustering")).toBeUndefined();

    const tight = deriveInsights({ ...base, accounts: [at(10), at(20), at(30)], series: [], rows: [] });
    expect(tight.find((i) => i.id === "clustering")?.figure).toBe("3 of 3");
  });

  it("ignores resets that have already passed and held-out accounts", () => {
    const past = account({ number: 1, usage: { fiveHour: { pct: 1, resetsAt: new Date(T0 - HOUR).toISOString() } } });
    const held = account({
      number: 2,
      usageStatus: "disabled",
      usage: { fiveHour: { pct: 1, resetsAt: new Date(T0 + 60_000).toISOString() } },
    });
    const live = account({ number: 3, usage: { fiveHour: { pct: 1, resetsAt: new Date(T0 + 120_000).toISOString() } } });
    const out = deriveInsights({ ...base, accounts: [past, held, live], series: [], rows: [] });
    expect(out.find((i) => i.id === "clustering")).toBeUndefined();
  });

  it("names idle accounts, but not when every account is idle", () => {
    const some = deriveInsights({ ...base, accounts: [], series: series([[1, 80], [2, 3]]), rows: [] });
    expect(some.find((i) => i.id === "idle")?.figure).toBe("1");

    // All idle is a quiet week, not an imbalance worth reporting.
    const all = deriveInsights({ ...base, accounts: [], series: series([[1, 4], [2, 3]]), rows: [] });
    expect(all.find((i) => i.id === "idle")).toBeUndefined();
  });

  it("ignores accounts with no readings when comparing load", () => {
    const withNull = [
      ...series([[1, 90], [2, 5]]),
      { ...series([[3, 0]])[0]!, mean: null, peak: null, last: null, runs: [] },
    ];
    const out = deriveInsights({ ...base, accounts: [], series: withNull, rows: [] });
    // Three accounts listed, two measured — the split is judged on the two.
    expect(out.find((i) => i.id === "concentration")?.figure).toBe("95%");
  });

  it("quotes the configured threshold rather than a hard-coded one", () => {
    const rows = [{ number: 1, name: "a", cells: [{ day: "d1", peak: 100, sampleCount: 3 }] }];
    const out = deriveInsights({ ...base, threshold: 72, accounts: [], series: [], rows });
    expect(out.find((i) => i.id === "saturation")?.detail).toContain("72%");
  });
});
