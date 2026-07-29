import { describe, expect, it } from "vitest";

import { accountBurn, formatRunway, pooledRunway } from "@/lib/runway";
import type { Account, Sample, Usage } from "@/types";

// Pinned. Every assertion here is about a projection, and a projection anchored
// to the wall clock is one that passes today and fails at 23:59.
const NOW = Date.parse("2026-07-29T12:00:00.000Z");

/** A reading of `bindingPct` taken `minutesAgo` before NOW. */
function sample(minutesAgo: number, bindingPct: number, accountKey = "a@example.com"): Sample {
  return {
    accountKey,
    timestamp: new Date(NOW - minutesAgo * 60_000).toISOString(),
    fiveHourPct: bindingPct,
    sevenDayPct: null,
    bindingPct,
    scoped: [],
  };
}

/** Two points two hours apart, so the rate is `(to - from) / 2` points/hour. */
function series(accountKey: string, from: number, to: number): Sample[] {
  return [sample(120, from, accountKey), sample(0, to, accountKey)];
}

/** A single binding window at `pct`, so headroom is `100 - pct`. */
function usageAt(pct: number): Usage {
  return { fiveHour: { pct } };
}

/** A fresh, healthy, idle account at 40% used. Tests override the field under test. */
function account(over: Partial<Account> = {}): Account {
  return {
    number: 1,
    email: "a@example.com",
    active: false,
    usageStatus: "ok",
    usage: usageAt(40),
    usageAgeSeconds: 30,
    ...over,
  };
}

/** Accounts are keyed by email throughout this suite. */
const keyFor = (a: Account) => a.email;

/** 20% two hours ago to 40% now: 10 points/hour, 60 points of headroom, 6h left. */
const RISING = [sample(120, 20), sample(60, 30), sample(0, 40)];

describe("accountBurn", () => {
  it("derives the slope and the runway it implies from a rising series", () => {
    expect(accountBurn(RISING, NOW)).toEqual({ pctPerHour: 10, secondsToLimit: 21_600 });
  });

  it("calls a flat series unknown rather than zero", () => {
    // 0 would read as "confirmed idle" and divide into an infinite runway.
    expect(accountBurn([sample(120, 40), sample(60, 40), sample(0, 40)], NOW)).toEqual({
      pctPerHour: null,
      secondsToLimit: null,
    });
  });

  it("calls a falling series unknown", () => {
    expect(accountBurn([sample(120, 60), sample(60, 50), sample(0, 40)], NOW)).toEqual({
      pctPerHour: null,
      secondsToLimit: null,
    });
  });

  it("cannot derive a rate from one reading or none", () => {
    expect(accountBurn([sample(0, 40)], NOW)).toEqual({ pctPerHour: null, secondsToLimit: null });
    expect(accountBurn([], NOW)).toEqual({ pctPerHour: null, secondsToLimit: null });
  });

  it("cannot derive a rate across a zero time span", () => {
    expect(accountBurn([sample(60, 20), sample(60, 30)], NOW)).toEqual({
      pctPerHour: null,
      secondsToLimit: null,
    });
  });

  it("measures the recent window, so an idle morning cannot flatten it", () => {
    const idleThenBusy = [sample(480, 0), sample(300, 0), ...RISING];
    // Across the whole array the slope would be 5 points/hour; only the last
    // three hours count, so it is 10.
    expect(accountBurn(idleThenBusy, NOW)).toEqual({ pctPerHour: 10, secondsToLimit: 21_600 });
  });

  it("is unknown when only pre-window samples exist", () => {
    expect(accountBurn([sample(480, 10), sample(300, 30)], NOW)).toEqual({
      pctPerHour: null,
      secondsToLimit: null,
    });
  });

  it("does not assume the samples arrive in order", () => {
    expect(accountBurn([...RISING].reverse(), NOW)).toEqual({
      pctPerHour: 10,
      secondsToLimit: 21_600,
    });
  });

  it("reports zero seconds for an account already at the limit", () => {
    // Measured exhaustion, not an unknown — 0 is honest here.
    expect(accountBurn([sample(60, 90), sample(0, 100)], NOW)).toEqual({
      pctPerHour: 10,
      secondsToLimit: 0,
    });
  });

  it("ignores unparseable timestamps rather than treating them as now", () => {
    const broken: Sample = { ...sample(0, 40), timestamp: "whenever" };
    expect(accountBurn([sample(120, 20), broken], NOW)).toEqual({
      pctPerHour: null,
      secondsToLimit: null,
    });
  });
});

describe("pooledRunway", () => {
  it("divides pooled headroom by the rate the live account is burning at", () => {
    const accounts = [
      account({ number: 1, active: true }),
      account({ number: 2, email: "b@example.com" }),
    ];
    const samples = new Map<string, Sample[]>([["a@example.com", RISING]]);

    // 60 + 60 points of headroom at 10 points/hour is 12 hours.
    expect(pooledRunway(accounts, samples, keyFor, NOW)).toEqual({
      seconds: 43_200,
      pctPerHour: 10,
      degraded: false,
      contributing: 2,
    });
  });

  it("counts idle spares at full headroom, so the pool far outlasts the live account", () => {
    // The case the whole model exists for: one account burning hard, three
    // barely-touched spares. A spare has no burn of its own and must not be
    // scored at zero for it.
    const burning = series("a@example.com", 60, 80);
    const accounts = [
      account({ number: 1, active: true, usage: usageAt(80) }),
      account({ number: 2, email: "b@example.com", usage: usageAt(5) }),
      account({ number: 3, email: "c@example.com", usage: usageAt(5) }),
      account({ number: 4, email: "d@example.com", usage: usageAt(5) }),
    ];
    const samples = new Map<string, Sample[]>([["a@example.com", burning]]);

    // 20 + 95 + 95 + 95 = 305 points at 10 points/hour.
    const estimate = pooledRunway(accounts, samples, keyFor, NOW);
    expect(estimate).toEqual({
      seconds: 109_800,
      pctPerHour: 10,
      degraded: false,
      contributing: 4,
    });

    // The live account alone is two hours from its limit; the pool is not.
    expect(accountBurn(burning, NOW).secondsToLimit).toBe(7_200);
    expect(estimate.seconds).toBeGreaterThan(7_200);
  });

  it("borrows the fastest rate seen anywhere when the live account is not burning", () => {
    const accounts = [
      account({ number: 1, active: true }),
      account({ number: 2, email: "b@example.com" }),
      account({ number: 3, email: "c@example.com" }),
    ];
    const samples = new Map<string, Sample[]>([
      ["b@example.com", series("b@example.com", 10, 40)], // 15 points/hour
      ["c@example.com", series("c@example.com", 20, 40)], // 10 points/hour
    ]);

    // 180 points at the fastest rate, 15/hour, is 12 hours. A borrowed rate is
    // a substitution, so the figure is labelled.
    expect(pooledRunway(accounts, samples, keyFor, NOW)).toEqual({
      seconds: 43_200,
      pctPerHour: 15,
      degraded: true,
      contributing: 3,
    });
  });

  it("borrows a rate when no account is marked live at all", () => {
    const accounts = [account({ number: 1 }), account({ number: 2, email: "b@example.com" })];
    const samples = new Map<string, Sample[]>([["a@example.com", RISING]]);

    expect(pooledRunway(accounts, samples, keyFor, NOW)).toEqual({
      seconds: 43_200,
      pctPerHour: 10,
      degraded: true,
      contributing: 2,
    });
  });

  it("takes the rate from the live account even when it is held out of the pool", () => {
    // Exclusions decide whose headroom is spendable, not who is burning.
    const accounts = [
      account({ number: 1, active: true, usageStatus: "disabled" }),
      account({ number: 2, email: "b@example.com" }),
    ];
    const samples = new Map<string, Sample[]>([["a@example.com", RISING]]);

    expect(pooledRunway(accounts, samples, keyFor, NOW)).toEqual({
      seconds: 21_600,
      pctPerHour: 10,
      degraded: false,
      contributing: 1,
    });
  });

  it("leaves out the headroom of accounts that cannot serve a request", () => {
    const live = account({ number: 1, active: true });
    const excluded = (["disabled", "reloginrequired", "foreigncredential"] as const).map(
      (usageStatus, i) => account({ number: i + 2, email: `x${i}@example.com`, usageStatus }),
    );
    const samples = new Map<string, Sample[]>([["a@example.com", RISING]]);

    // 60 points, not 240. Their absence is not a measurement gap either, so
    // the estimate stays undegraded.
    expect(pooledRunway([live, ...excluded], samples, keyFor, NOW)).toEqual({
      seconds: 21_600,
      pctPerHour: 10,
      degraded: false,
      contributing: 1,
    });
  });

  it("is unknown, never zero, when nothing is burning anywhere", () => {
    const accounts = [
      account({ number: 1, active: true }),
      account({ number: 2, email: "b@example.com" }),
    ];
    const samples = new Map<string, Sample[]>([
      ["a@example.com", [sample(120, 40), sample(0, 40)]],
    ]);

    // Genuinely idle. The headroom is known, the rate is not, so the runway is
    // unknown rather than infinite or zero.
    expect(pooledRunway(accounts, samples, keyFor, NOW)).toEqual({
      seconds: null,
      pctPerHour: null,
      degraded: false,
      contributing: 2,
    });
  });

  it("is unknown when there is no history to derive any rate from", () => {
    const accounts = [account({ active: true })];

    expect(pooledRunway(accounts, new Map(), () => undefined, NOW)).toEqual({
      seconds: null,
      pctPerHour: null,
      degraded: false,
      contributing: 1,
    });
  });

  it("degrades and undercounts when a usable account has no usage reading", () => {
    const accounts = [
      account({ number: 1, active: true }),
      account({ number: 2, email: "b@example.com", usage: undefined }),
    ];
    const samples = new Map<string, Sample[]>([["a@example.com", RISING]]);

    expect(pooledRunway(accounts, samples, keyFor, NOW)).toEqual({
      seconds: 21_600,
      pctPerHour: 10,
      degraded: true,
      contributing: 1,
    });
  });

  it("degrades when a contributing account's reading is stale", () => {
    const accounts = [account({ active: true, usageAgeSeconds: 900 })];
    const samples = new Map<string, Sample[]>([["a@example.com", RISING]]);

    expect(pooledRunway(accounts, samples, keyFor, NOW)).toEqual({
      seconds: 21_600,
      pctPerHour: 10,
      degraded: true,
      contributing: 1,
    });
  });

  it("degrades when an age is missing entirely", () => {
    const accounts = [account({ active: true, usageAgeSeconds: undefined })];
    const samples = new Map<string, Sample[]>([["a@example.com", RISING]]);

    expect(pooledRunway(accounts, samples, keyFor, NOW).degraded).toBe(true);
  });

  it("degrades on any usage status other than ok", () => {
    const accounts = [account({ active: true, usageStatus: "unavailable" })];
    const samples = new Map<string, Sample[]>([["a@example.com", RISING]]);

    expect(pooledRunway(accounts, samples, keyFor, NOW)).toEqual({
      seconds: 21_600,
      pctPerHour: 10,
      degraded: true,
      contributing: 1,
    });
  });

  it("keeps an exhausted account in the pool at zero headroom", () => {
    const accounts = [
      account({ number: 1, active: true }),
      account({ number: 2, email: "b@example.com", usage: usageAt(100) }),
    ];
    const samples = new Map<string, Sample[]>([["a@example.com", RISING]]);

    expect(pooledRunway(accounts, samples, keyFor, NOW)).toEqual({
      seconds: 21_600,
      pctPerHour: 10,
      degraded: false,
      contributing: 2,
    });
  });

  it("is unknown when there are no accounts to pool", () => {
    expect(pooledRunway([], new Map(), keyFor, NOW)).toEqual({
      seconds: null,
      pctPerHour: null,
      degraded: false,
      contributing: 0,
    });
  });
});

describe("formatRunway", () => {
  it("uses the same buckets as the reset countdown", () => {
    expect(formatRunway(2 * 86_400 + 3 * 3_600)).toBe("2d 3h");
    expect(formatRunway(6 * 3_600 + 20 * 60)).toBe("6h 20m");
    expect(formatRunway(18 * 60)).toBe("18m");
  });

  it("switches bucket exactly on the hour and day boundaries", () => {
    expect(formatRunway(3_599)).toBe("59m");
    expect(formatRunway(3_600)).toBe("1h 0m");
    expect(formatRunway(86_399)).toBe("23h 59m");
    expect(formatRunway(86_400)).toBe("1d 0h");
  });

  it("reports a floor past the horizon rather than noise-driven precision", () => {
    // 7d exactly is still a measurement; a second past it is not.
    expect(formatRunway(7 * 86_400)).toBe("7d 0h");
    expect(formatRunway(7 * 86_400 + 1)).toBe("> 7d");
    // The case that motivated the cap: four accounts against 0.01 points/hour
    // of measurement noise, which used to render as "1583d 8h".
    expect(formatRunway((380 / 0.01) * 3_600)).toBe("> 7d");
  });

  it("says 'now' for a pool that is already spent", () => {
    expect(formatRunway(0)).toBe("now");
    expect(formatRunway(-60)).toBe("now");
  });

  it("says unknown rather than inventing a duration", () => {
    expect(formatRunway(null)).toBe("unknown");
    expect(formatRunway(Number.POSITIVE_INFINITY)).toBe("unknown");
    expect(formatRunway(Number.NaN)).toBe("unknown");
  });
});
