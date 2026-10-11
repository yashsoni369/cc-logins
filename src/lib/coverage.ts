/**
 * Forward-looking readouts for Home, Settings and the popover: which account
 * auto-switch would pick next, when the account in use is projected to hit a
 * limit, and the per-account lanes of the coverage timeline.
 *
 * Everything here is a *prediction* and every caller labels it as one. The
 * backend's daemon phase stays the authority on what auto-switch is actually
 * doing; nothing in this module triggers or schedules anything.
 */

import { accountBurn } from "@/lib/runway";
import {
  bindingUtilisation,
  bindingWindow,
  headroom,
  isApiAccount,
  isEnterprise,
  isOutOfCredit,
  type Account,
  type Sample,
  type Settings,
} from "@/types";

export type Strategy = Settings["strategy"];

/**
 * Mirrors `Account::is_automatic_target` (src-tauri/src/model.rs): never the
 * account in use, only an `ok` reading, and only with known, positive headroom.
 */
export function isAutomaticTarget(account: Account): boolean {
  if (account.active || account.usageStatus !== "ok") return false;
  const h = headroom(account.usage);
  return h !== null && h > 0;
}

/**
 * The last resort, mirroring `pick_pay_as_you_go_fallback`: an API account,
 * which spends real money, only once every subscription that could be used
 * (the one in use included) is *known* to be at its limit. An unknown reading
 * is never enough.
 */
function payAsYouGoFallback(accounts: Account[]): Account | null {
  const usable = (a: Account) =>
    a.active || (a.usageStatus !== "disabled" && a.usageStatus !== "reloginrequired");
  const subscriptions = accounts.filter((a) => !isApiAccount(a) && usable(a));
  if (subscriptions.length === 0) return null;
  const allSpent = subscriptions.every((a) => {
    const h = headroom(a.usage);
    return h !== null && h <= 0;
  });
  if (!allSpent) return null;
  return (
    accounts.find((a) => !a.active && isApiAccount(a) && a.usageStatus === "payAsYouGo" && !isOutOfCredit(a)) ?? null
  );
}

/** Epoch ms of the weekly reset, or +∞ when unknown or already past (as `seven_day_reset_ts`). */
function weeklyResetMs(account: Account, now: number): number {
  const iso = account.usage?.sevenDay?.resetsAt;
  const ms = iso ? Date.parse(iso) : Number.NaN;
  return Number.isFinite(ms) && ms > now ? ms : Number.POSITIVE_INFINITY;
}

/**
 * The account auto-switch would move to under `strategy`, mirroring
 * `switcher::pick_target`. `accounts` must be in the backend's rotation order,
 * which is the order the snapshot lists them in.
 */
export function predictTarget(accounts: Account[], strategy: Strategy, now: number): Account | null {
  const candidates = accounts.filter(isAutomaticTarget);
  if (candidates.length === 0) return payAsYouGoFallback(accounts);
  if (strategy === "next-available") return candidates[0] ?? null;

  let best: Account | null = null;
  for (const candidate of candidates) {
    if (!best) {
      best = candidate;
      continue;
    }
    const gain = (headroom(candidate.usage) ?? 0) - (headroom(best.usage) ?? 0);
    if (strategy === "consume-first") {
      const a = weeklyResetMs(candidate, now);
      const b = weeklyResetMs(best, now);
      if (a < b || (a === b && gain > 0)) best = candidate;
    } else if (gain > 0) {
      // Ties keep the earlier slot, like the backend's `>=` comparison.
      best = candidate;
    }
  }
  return best;
}

/** The order after moving slot `number` one place up (-1) or down (+1). */
export function moveInOrder(accounts: Account[], number: number, direction: -1 | 1): number[] {
  const ids = accounts.map((a) => a.number);
  const i = ids.indexOf(number);
  const j = i + direction;
  if (i === -1 || j < 0 || j >= ids.length) return ids;
  const swapped = ids[j] as number;
  ids[j] = number;
  ids[i] = swapped;
  return ids;
}

export interface Projection {
  /** Epoch ms the limiting window is projected to reach the target %, or null when unknowable. */
  at: number | null;
  /** Epoch ms that window resets, when known. */
  resetsAt: number | null;
  /** True when the projection lands before the reset — it will actually cut you off. */
  beforeReset: boolean;
}

/**
 * When the account's limiting window reaches `atPct` at its recent burn rate.
 *
 * Reuses `accountBurn` so this and the pooled runway can never disagree on
 * the rate. Flat, falling or unmeasured usage is unknown — never "lasts
 * forever" — and a monthly spend cap is out of scope for an hourly
 * projection, exactly as in `pooledRunway`.
 */
export function projectLimit(account: Account, samples: Sample[], now: number, atPct = 100): Projection {
  const binding = bindingWindow(account.usage);
  const resetMs = binding?.resetsAt ? Date.parse(binding.resetsAt) : Number.NaN;
  const resetsAt = Number.isFinite(resetMs) ? resetMs : null;
  const unknown: Projection = { at: null, resetsAt, beforeReset: false };
  if (isEnterprise(account.usage)) return unknown;
  const util = bindingUtilisation(account.usage);
  if (util === null) return unknown;
  if (util >= atPct) return { at: now, resetsAt, beforeReset: resetsAt === null || now < resetsAt };
  const { pctPerHour } = accountBurn(samples, now);
  if (pctPerHour === null || !(pctPerHour > 0)) return unknown;
  const at = now + ((atPct - util) / pctPerHour) * 3_600_000;
  return { at, resetsAt, beforeReset: resetsAt === null || at < resetsAt };
}

export type LaneState = "free" | "inUse" | "limited";

export interface LaneSegment {
  state: LaneState;
  /** Hours from now. */
  from: number;
  to: number;
}

export interface Lane {
  account: Account;
  segments: LaneSegment[];
  /** Hours from now of the limiting window's reset, when inside the horizon. */
  reset: number | null;
  /** Hours from now of the projected limit, for the account in use. */
  limit: number | null;
  /** Held out, needing sign-in, or mismatched — drawn but dimmed. */
  unavailable: boolean;
  /** Usage unknown — drawn as unknown, never as free. */
  unknown: boolean;
  /** A Console API key: billed per request, so there is no quota to draw. */
  payg?: boolean;
}

export interface Handoff {
  from: number;
  to: number;
  /** Hours from now. */
  at: number;
}

export interface CoveragePlan {
  lanes: Lane[];
  handoff: Handoff | null;
}

const UNAVAILABLE = new Set(["disabled", "reloginrequired", "foreigncredential"]);

/**
 * The next `horizonHours` for every account, as simple segments.
 *
 * Deliberately modest: only the account in use is projected, from its own
 * burn. Every other account is drawn as it stands now until its reset,
 * because there is no honest way to predict usage nobody is generating yet.
 * A handoff is drawn only when auto-switch is on, to the account
 * `predictTarget` names — the rule the daemon itself applies.
 */
export function coveragePlan(options: {
  accounts: Account[];
  activeSamples: Sample[];
  now: number;
  horizonHours: number;
  autoSwitch: boolean;
  threshold: number;
  strategy: Strategy;
}): CoveragePlan {
  const { accounts, activeSamples, now, horizonHours, autoSwitch, threshold, strategy } = options;
  const toHours = (ms: number) => (ms - now) / 3_600_000;
  const clamp = (h: number) => Math.max(0, Math.min(horizonHours, h));
  const active = accounts.find((a) => a.active) ?? null;
  const limitAt = active ? projectLimit(active, activeSamples, now, 100) : null;
  const switchAt = active && autoSwitch ? projectLimit(active, activeSamples, now, threshold) : null;
  const target = active && autoSwitch ? predictTarget(accounts, strategy, now) : null;

  let handoff: Handoff | null = null;
  if (active && target && switchAt?.at != null && switchAt.beforeReset && toHours(switchAt.at) <= horizonHours) {
    handoff = { from: active.number, to: target.number, at: clamp(toHours(switchAt.at)) };
  }

  const lanes = accounts.map((account): Lane => {
    // In use, or about to be: drawn as such. Otherwise an empty track, since
    // there is no limit to run into, only money to spend.
    if (isApiAccount(account)) {
      const segments: LaneSegment[] = account.active
        ? [{ state: "inUse", from: 0, to: horizonHours }]
        : handoff?.to === account.number
          ? [{ state: "inUse", from: handoff.at, to: horizonHours }]
          : [];
      return {
        account,
        segments,
        reset: null,
        limit: null,
        unavailable: account.usageStatus === "disabled",
        unknown: false,
        payg: true,
      };
    }
    const unavailable = UNAVAILABLE.has(account.usageStatus);
    const util = bindingUtilisation(account.usage);
    const resetIso = bindingWindow(account.usage)?.resetsAt;
    const resetMs = resetIso ? Date.parse(resetIso) : Number.NaN;
    const resetH = Number.isFinite(resetMs) ? toHours(resetMs) : null;
    const reset = resetH !== null && resetH > 0 && resetH <= horizonHours ? resetH : null;
    if (util === null) return { account, segments: [], reset, limit: null, unavailable, unknown: true };

    const segments: LaneSegment[] = [];
    const atLimitNow = util >= 100;
    let limit: number | null = null;

    if (atLimitNow) {
      segments.push({ state: "limited", from: 0, to: reset ?? horizonHours });
      if (reset !== null) segments.push({ state: "free", from: reset, to: horizonHours });
    } else if (account.active) {
      const limitH = limitAt?.at != null && limitAt.beforeReset ? clamp(toHours(limitAt.at)) : null;
      if (handoff) {
        segments.push({ state: "inUse", from: 0, to: handoff.at }, { state: "free", from: handoff.at, to: horizonHours });
      } else if (limitH !== null) {
        limit = limitH;
        segments.push({ state: "inUse", from: 0, to: limitH }, { state: "limited", from: limitH, to: reset ?? horizonHours });
        if (reset !== null && reset > limitH) segments.push({ state: "free", from: reset, to: horizonHours });
      } else {
        segments.push({ state: "inUse", from: 0, to: horizonHours });
      }
    } else if (handoff?.to === account.number) {
      segments.push({ state: "free", from: 0, to: handoff.at }, { state: "inUse", from: handoff.at, to: horizonHours });
    } else {
      segments.push({ state: "free", from: 0, to: horizonHours });
    }

    return { account, segments: segments.filter((s) => s.to > s.from), reset, limit, unavailable, unknown: false };
  });

  return { lanes, handoff };
}

/**
 * Where weekly utilisation *should* be by now (0..100), for the pace tick.
 * The server's own `expectedPct` wins; otherwise it is derived from how much
 * of the 7-day window has elapsed.
 */
export function weeklyPaceMark(usage: Account["usage"], now: number): number | null {
  const week = usage?.sevenDay;
  if (!week) return null;
  if (week.expectedPct != null && Number.isFinite(week.expectedPct)) {
    return Math.max(0, Math.min(100, week.expectedPct));
  }
  const ms = week.resetsAt ? Date.parse(week.resetsAt) : Number.NaN;
  if (!Number.isFinite(ms)) return null;
  const remaining = (ms - now) / (7 * 86_400_000);
  if (remaining < 0 || remaining > 1) return null;
  return (1 - remaining) * 100;
}
