import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { formatClock, formatCountdown, formatInstant, useNow } from "@/lib/time";

// Local noon, so every offset below stays inside the same local day whatever
// timezone the runner is in. Locales are always passed explicitly for the same
// reason — none of these assertions may depend on the machine.
const NOW = new Date(2026, 6, 29, 12, 0, 0).getTime();

/** Local wall-clock instant as ISO, so day boundaries land where the test says. */
function at(year: number, month: number, day: number, hour: number, minute: number): string {
  return new Date(year, month, day, hour, minute, 0).toISOString();
}

function after(seconds: number): string {
  return new Date(NOW + seconds * 1000).toISOString();
}

/** ICU 72+ puts a narrow no-break space before AM/PM; fold it for comparison. */
function norm(value: string | null): string | null {
  return value === null ? null : value.replace(/[\u202f\u00a0]/g, " ");
}

describe("formatCountdown", () => {
  it("mirrors the Rust buckets", () => {
    expect(formatCountdown(after(2 * 86_400 + 5 * 3_600), NOW)).toBe("2d 5h");
    expect(formatCountdown(after(3 * 3_600 + 12 * 60), NOW)).toBe("3h 12m");
    expect(formatCountdown(after(12 * 60), NOW)).toBe("12m");
  });

  it("switches bucket exactly on the hour and day boundaries", () => {
    expect(formatCountdown(after(3_599), NOW)).toBe("59m");
    expect(formatCountdown(after(3_600), NOW)).toBe("1h 0m");
    expect(formatCountdown(after(86_399), NOW)).toBe("23h 59m");
    expect(formatCountdown(after(86_400), NOW)).toBe("1d 0h");
  });

  it("says 'now' once the reset has arrived, however far past", () => {
    expect(formatCountdown(after(0), NOW)).toBe("now");
    expect(formatCountdown(after(-300), NOW)).toBe("now");
    expect(formatCountdown(after(-9 * 86_400), NOW)).toBe("now");
    // Under a minute still remaining is "0m", exactly as Rust reports it.
    expect(formatCountdown(after(30), NOW)).toBe("0m");
  });

  it("returns null rather than inventing a value", () => {
    expect(formatCountdown(undefined, NOW)).toBeNull();
    expect(formatCountdown("", NOW)).toBeNull();
    expect(formatCountdown("soon", NOW)).toBeNull();
  });
});

describe("formatClock", () => {
  it("shows time only on the same local day", () => {
    const reset = at(2026, 6, 29, 15, 0);
    expect(norm(formatClock(reset, "24h", NOW, "en-US"))).toBe("15:00");
    expect(norm(formatClock(reset, "12h", NOW, "en-US"))).toBe("3:00 PM");
  });

  it("lets the locale decide under 'system'", () => {
    const reset = at(2026, 6, 29, 15, 0);
    expect(norm(formatClock(reset, "system", NOW, "en-US"))).toBe("3:00 PM");
    expect(norm(formatClock(reset, "system", NOW, "en-GB"))).toBe("15:00");
  });

  it("pads the 24h hour and never renders midnight as 24:00", () => {
    expect(formatClock(at(2026, 6, 29, 8, 5), "24h", NOW, "en-US")).toBe("08:05");
    expect(formatClock(at(2026, 6, 29, 0, 0), "24h", NOW, "en-US")).toBe("00:00");
  });

  it("adds month and day for any other day, in reset_clock_string's shape", () => {
    const tomorrow = at(2026, 6, 30, 18, 0);
    expect(formatClock(tomorrow, "24h", NOW, "en-US")).toBe("Jul 30 18:00");
    expect(norm(formatClock(tomorrow, "12h", NOW, "en-US"))).toBe("Jul 30 6:00 PM");
    expect(formatClock(at(2026, 7, 1, 1, 29), "24h", NOW, "en-US")).toBe("Aug 1 01:29");
    // Yesterday is "a different day" too, not a bare time.
    expect(formatClock(at(2026, 6, 28, 23, 45), "24h", NOW, "en-US")).toBe("Jul 28 23:45");
  });

  it("returns null for unknown input", () => {
    expect(formatClock(undefined, "24h", NOW, "en-US")).toBeNull();
    expect(formatClock("", "24h", NOW, "en-US")).toBeNull();
    expect(formatClock("whenever", "24h", NOW, "en-US")).toBeNull();
  });
});

describe("formatInstant", () => {
  it("carries the full date alongside the chosen clock", () => {
    const instant = at(2026, 6, 29, 15, 0);
    expect(norm(formatInstant(instant, "24h", "en-US"))).toMatch(/^Jul 29, 2026\b.*\b15:00$/);
    expect(norm(formatInstant(instant, "12h", "en-US"))).toMatch(/^Jul 29, 2026\b.*\b3:00 PM$/);
  });

  it("returns null for unknown input", () => {
    expect(formatInstant(undefined, "24h", "en-US")).toBeNull();
    expect(formatInstant("nope", "24h", "en-US")).toBeNull();
  });
});

describe("useNow", () => {
  afterEach(() => vi.useRealTimers());

  it("shares one interval across subscribers and clears it on the last unmount", () => {
    vi.useFakeTimers();
    const setInterval = vi.spyOn(window, "setInterval");
    const clearInterval = vi.spyOn(window, "clearInterval");

    const first = renderHook(() => useNow(1_000));
    const second = renderHook(() => useNow(1_000));
    expect(setInterval).toHaveBeenCalledTimes(1);

    first.unmount();
    expect(clearInterval).not.toHaveBeenCalled();

    second.unmount();
    expect(clearInterval).toHaveBeenCalledTimes(1);
  });

  it("advances with the clock", () => {
    vi.useFakeTimers();
    vi.setSystemTime(NOW);
    const { result, unmount } = renderHook(() => useNow(1_000));

    expect(result.current).toBe(NOW);
    act(() => void vi.advanceTimersByTime(1_000));
    expect(result.current).toBe(NOW + 1_000);

    unmount();
  });
});
