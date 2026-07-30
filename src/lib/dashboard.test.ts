import { describe, expect, it } from "vitest";

import {
  RANGES,
  buildFleetSeries,
  deriveInsights,
  isRangeKey,
  loadBalanceGrid,
  oscillations,
  pooledHeadroom,
  runsOf,
  thin,
  type Pt,
} from "./dashboard";
import type { Account, DayStat, Sample } from "@/types";

describe("oscillations", () => {
  const run = (values: number[]): Pt[] => values.map((v, i) => ({ x: i / 10, v }));

  it("counts a steady climb as no reversal, however many points it has", () => {
    expect(oscillations([run(Array.from({ length: 500 }, (_, i) => i / 5))])).toBe(0);
  });

  it("counts each turn in a sawtooth", () => {
    // up, down, up, down -> three reversals
    expect(oscillations([run([0, 50, 0, 50, 0])])).toBe(3);
  });

  it("ignores wobble under the noise floor", () => {
    // A flat quota read repeatedly is not thirty direction changes.
    const jitter = Array.from({ length: 30 }, (_, i) => 40 + (i % 2));
    expect(oscillations([run(jitter)])).toBe(0);
  });

  it("is zero for an empty or single-point series", () => {
    expect(oscillations([])).toBe(0);
    expect(oscillations([run([42])])).toBe(0);
  });
});

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
  it("plots raw samples only where individual cycles are still readable", () => {
    // A day is always drawable and a month never is: prune deletes samples
    // once they age past retention, so a long range asked for in samples comes
    // back progressively emptier. A week genuinely depends on how busy it was,
    // so it decides from the data rather than in advance.
    expect(RANGES["24h"].source).toBe("samples");
    expect(RANGES["7d"].source).toBe("auto");
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

  /*
   * "auto" is the 7-day range's answer to a question that has no fixed one:
   * a quiet week of samples is legible and worth showing raw, a busy one is
   * thirty overlaid sawtooth cycles.
   */
  describe("choosing a source automatically", () => {
    const now = Date.parse("2026-07-20T12:00:00Z");
    const daily = new Map([["key-1", [day("2026-07-18", 60, 0, 30), day("2026-07-19", 70, 0, 35)]]]);
    const one = [account({ number: 1 })];

    /** A sawtooth: `cycles` climbs, each dropping back to nothing. */
    const sawtooth = (cycles: number) => {
      const out: Sample[] = [];
      let minute = -cycles * 300;
      for (let c = 0; c < cycles; c++) {
        for (const v of [5, 40, 75, 95]) out.push(sample(minute++, v));
        out.push(sample(minute++, 3));
      }
      return out;
    };

    it("keeps raw samples when the week was quiet enough to read", () => {
      const calm = [sample(-4000, 10), sample(-3000, 25), sample(-2000, 45), sample(-1000, 60)];
      const series = buildFleetSeries(one, keyFor, new Map([["key-1", calm]]), daily, RANGES["7d"], 240, now);
      // Values only a raw series carries; the daily rollups here are 30/35.
      expect(series[0]?.runs.flat().map((p) => p.v)).toEqual([10, 25, 45, 60]);
    });

    it("drops the whole fleet to rollups once one account scribbles", () => {
      const series = buildFleetSeries(
        one,
        keyFor,
        new Map([["key-1", sawtooth(15)]]),
        daily,
        RANGES["7d"],
        240,
        now,
      );
      expect(series[0]?.runs.flat().map((p) => p.v)).toEqual([30, 35]);
    });

    it("never mixes sources across accounts sharing one axis", () => {
      // A raw sawtooth beside a daily average invites a comparison between two
      // different measurements, so the busy account decides for everyone.
      const two = [account({ number: 1 }), account({ number: 2 })];
      const samples = new Map([
        ["key-1", [sample(-2000, 10), sample(-1000, 20)]],
        ["key-2", sawtooth(15)],
      ]);
      const bothDaily = new Map([
        ["key-1", [day("2026-07-19", 50, 0, 25)]],
        ["key-2", [day("2026-07-19", 90, 0, 45)]],
      ]);
      const series = buildFleetSeries(two, keyFor, samples, bothDaily, RANGES["7d"], 240, now);
      expect(series[0]?.runs.flat().map((p) => p.v)).toEqual([25]);
      expect(series[1]?.runs.flat().map((p) => p.v)).toEqual([45]);
    });

    it("falls back to rollups when the range holds no samples at all", () => {
      // Samples past retention are deleted, but their rollups survive.
      const series = buildFleetSeries(one, keyFor, new Map(), daily, RANGES["7d"], 240, now);
      expect(series[0]?.runs.flat().map((p) => p.v)).toEqual([30, 35]);
    });
  });

  it("clips daily history to the selected range", () => {
    // The daily map is fetched at the longest span any panel needs — the
    // load-balance grid always wants a month — so it holds more than the chart
    // asked for. Unclipped, "7 days" drew a month under a 7-day axis.
    const now = Date.parse("2026-07-20T00:00:00Z");
    const daily = new Map([
      [
        "key-1",
        [
          day("2026-06-25", 90, 0, 90), // 25 days back — outside a 7-day range
          day("2026-07-18", 40, 0, 40),
          day("2026-07-19", 50, 0, 50),
        ],
      ],
    ]);
    const week = buildFleetSeries(accounts, keyFor, new Map(), daily, RANGES["7d"], 240, now);
    expect(week[0]?.runs.flat().map((p) => p.v)).toEqual([40, 50]);

    // The same data under a 30-day range keeps the older day.
    const month = buildFleetSeries(accounts, keyFor, new Map(), daily, RANGES["30d"], 240, now);
    expect(month[0]?.runs.flat().map((p) => p.v)).toContain(90);
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

  it("reports the capacity the pooled figure is a fraction of", () => {
    // Without this the bar had nothing absolute to be drawn against, so it
    // filled its container whatever the numbers said: 12 points of headroom
    // across three accounts drew exactly as long as 290.
    const result = pooledHeadroom([
      account({ number: 1, usage: { sevenDay: { pct: 40 } } }),
      account({ number: 2, usage: { sevenDay: { pct: 90 } } }),
      account({ number: 3, usage: { sevenDay: { pct: 70 } } }),
    ]);
    expect(result.capacity).toBe(300);
    expect(result.pooled).toBe(100);
    expect(result.spent).toBe(200);
  });

  it("counts capacity only for accounts the switcher can reach", () => {
    const result = pooledHeadroom([
      account({ number: 1, usage: { sevenDay: { pct: 40 } } }),
      account({ number: 2, usageStatus: "disabled", usage: { sevenDay: { pct: 0 } } }),
      account({ number: 3 }),
    ]);
    // One usable account, so the bar's full length is one account's worth.
    expect(result.capacity).toBe(100);
    expect(result.pooled).toBe(60);
    expect(result.spent).toBe(40);
  });

  it("never reports negative spend for an account past its limit", () => {
    const result = pooledHeadroom([account({ number: 1, usage: { sevenDay: { pct: 130 } } })]);
    expect(result.spent).toBe(100);
    expect(result.pooled).toBe(0);
  });

  it("handles an empty fleet", () => {
    expect(pooledHeadroom([])).toMatchObject({
      pooled: 0,
      capacity: 0,
      spent: 0,
      usable: 0,
      total: 0,
      segments: [],
    });
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

  it("counts distinct days, not account-days", () => {
    // Two accounts topping out on the same afternoon is one bad day. Summing
    // per-account cells reported it as two and overstated how often this
    // happens — and the headline says "days".
    const rows = [
      { number: 1, name: "a", cells: [{ day: "d1", peak: 100, sampleCount: 5 }, { day: "d2", peak: 40, sampleCount: 5 }] },
      { number: 2, name: "b", cells: [{ day: "d1", peak: 99, sampleCount: 5 }] },
    ];
    const found = deriveInsights({ ...base, accounts: [], series: [], rows }).find((i) => i.id === "saturation");
    expect(found?.figure).toBe("1");
    expect(found?.headline).toContain("day ");
  });

  it("does count separate days separately", () => {
    const rows = [
      { number: 1, name: "a", cells: [{ day: "d1", peak: 100, sampleCount: 5 }] },
      { number: 2, name: "b", cells: [{ day: "d2", peak: 100, sampleCount: 5 }] },
    ];
    const found = deriveInsights({ ...base, accounts: [], series: [], rows }).find((i) => i.id === "saturation");
    expect(found?.figure).toBe("2");
    expect(found?.headline).toContain("days");
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
