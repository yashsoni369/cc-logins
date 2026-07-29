/**
 * Every time string in the UI comes from here.
 *
 * Three formatters had grown up independently and disagreed on screen: the
 * Rust side's hard-coded 24h `clock`, the popover's locale-driven `Intl` call,
 * and the details panel's `toLocaleString`. This module is the only one that
 * decides now, and its countdown buckets mirror
 * `src-tauri/src/oauth.rs::reset_strings` exactly so the two sides cannot
 * drift apart again.
 *
 * Every formatter takes an injectable `now` rather than reading the clock
 * itself — that is what keeps the components rendering countdowns testable.
 */

import { useEffect, useState } from "react";

/** How a time of day is rendered. `"system"` defers to the OS locale. */
export type ClockFormat = "system" | "12h" | "24h";

/** Epoch millis, or null when the string is not a usable instant. */
function parse(iso: string | undefined): number | null {
  if (!iso) return null;
  const ms = Date.parse(iso);
  return Number.isNaN(ms) ? null : ms;
}

/**
 * Time left until `resetsAt`: `"3d 4h"`, `"4h 21m"`, `"12m"`. Buckets mirror
 * `reset_strings`; the one deviation is `"now"` for an elapsed reset, where
 * Rust says `"0m"`. Null for unknown input — never a fabricated value.
 */
export function formatCountdown(resetsAt: string | undefined, now = Date.now()): string | null {
  const target = parse(resetsAt);
  if (target === null) return null;

  const remaining = Math.max(0, Math.floor((target - now) / 1000));
  if (remaining === 0) return "now";

  const days = Math.floor(remaining / 86_400);
  const rem = remaining % 86_400;
  const hours = Math.floor(rem / 3_600);
  const minutes = Math.floor((rem % 3_600) / 60);

  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes}m`;
  return `${minutes}m`;
}

/**
 * `"system"` leaves `hour12` undefined so the locale decides. The 24h case
 * sets `hourCycle` instead of `hour12: false`, which renders midnight as
 * `"24:00"` in some locales.
 */
function timeOptions(fmt: ClockFormat): Intl.DateTimeFormatOptions {
  if (fmt === "12h") return { hour: "numeric", minute: "2-digit", hour12: true };
  if (fmt === "24h") return { hour: "2-digit", minute: "2-digit", hourCycle: "h23" };
  return { hour: "numeric", minute: "2-digit" };
}

function sameLocalDay(a: Date, b: Date): boolean {
  return (
    a.getFullYear() === b.getFullYear() &&
    a.getMonth() === b.getMonth() &&
    a.getDate() === b.getDate()
  );
}

/**
 * Time of day: `"20:39"` / `"8:39 PM"` when it lands on the same local day as
 * `now`, else `"Aug 1 01:29"` — the shape `reset_clock_string` produces, so
 * date and time are joined by a plain space rather than the comma `Intl`
 * would insert. `locale` exists for tests; production omits it so the OS wins.
 */
export function formatClock(
  iso: string | undefined,
  fmt: ClockFormat,
  now = Date.now(),
  locale?: string,
): string | null {
  const ms = parse(iso);
  if (ms === null) return null;

  const at = new Date(ms);
  const time = new Intl.DateTimeFormat(locale, timeOptions(fmt)).format(at);
  if (sameLocalDay(at, new Date(now))) return time;

  const day = new Intl.DateTimeFormat(locale, { month: "short", day: "numeric" }).format(at);
  return `${day} ${time}`;
}

/**
 * Full instant for the expanded details panel, e.g. `"Jul 29, 2026, 3:00 PM"`.
 * Spelled out in components rather than `dateStyle`/`timeStyle` because those
 * cannot be combined with the `hourCycle` the 24h case needs.
 */
export function formatInstant(
  iso: string | undefined,
  fmt: ClockFormat,
  locale?: string,
): string | null {
  const ms = parse(iso);
  if (ms === null) return null;
  return new Intl.DateTimeFormat(locale, {
    year: "numeric",
    month: "short",
    day: "numeric",
    ...timeOptions(fmt),
  }).format(new Date(ms));
}

interface Ticker {
  timer: number;
  subscribers: Set<(now: number) => void>;
}

// One interval per distinct period, however many countdowns are mounted.
const tickers = new Map<number, Ticker>();

function subscribe(intervalMs: number, notify: (now: number) => void): () => void {
  let ticker = tickers.get(intervalMs);
  if (!ticker) {
    const created: Ticker = { timer: 0, subscribers: new Set() };
    created.timer = window.setInterval(() => {
      const now = Date.now();
      for (const subscriber of created.subscribers) subscriber(now);
    }, intervalMs);
    tickers.set(intervalMs, created);
    ticker = created;
  }
  ticker.subscribers.add(notify);

  return () => {
    const live = tickers.get(intervalMs);
    if (!live) return;
    live.subscribers.delete(notify);
    if (live.subscribers.size === 0) {
      window.clearInterval(live.timer);
      tickers.delete(intervalMs);
    }
  };
}

/**
 * Shared wall clock for anything that re-renders as time passes. Subscribers
 * on the same period ride one interval — ten mounted countdowns must not mean
 * ten timers — and the last unmount clears it.
 */
export function useNow(intervalMs = 30_000): number {
  const [now, setNow] = useState(Date.now);
  useEffect(() => subscribe(intervalMs, setNow), [intervalMs]);
  return now;
}
