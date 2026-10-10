import { useEffect, useState, type FormEvent } from "react";

import UsageMeter from "../UsageMeter";
import { refreshBilling, setBillingKey, setBillingLimits } from "../../lib/api";
import { useClockFormat } from "../../lib/clockFormat";
import { formatClock } from "../../lib/time";
import { creditUtilisation, formatUsd, type Account, type DailySpend } from "../../types";

/** "Nov 1", in the viewer's locale, for a UTC instant. */
function dayLabel(iso: string | undefined): string | null {
  if (!iso) return null;
  const ms = Date.parse(iso);
  if (!Number.isFinite(ms)) return null;
  return new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", timeZone: "UTC" }).format(ms);
}

/** An input's text for a stored dollar figure: blank when unset. */
function inputValue(value: number | undefined): string {
  return value == null ? "" : String(value);
}

/** `""` → `null`; "$1,200.50" → 1200.5; anything else → `undefined` (invalid). */
function parseUsd(text: string): number | null | undefined {
  const cleaned = text.replace(/[$,\s]/g, "");
  if (cleaned === "") return null;
  const value = Number(cleaned);
  return Number.isFinite(value) && value >= 0 ? value : undefined;
}

/** One bar per day, scaled to the busiest day. Each bar's title carries its figure. */
export function DailySpendBars({ daily }: { daily: DailySpend[] }) {
  const max = Math.max(0, ...daily.map((d) => d.usd));
  const total = daily.reduce((sum, d) => sum + d.usd, 0);
  return (
    <div className="spend-bars" role="img" aria-label={`Daily spend over the last ${daily.length} days: ${formatUsd(total)} in total`}>
      {daily.map((d) => (
        <span
          key={d.day}
          className="spend-bar"
          title={`${dayLabel(`${d.day}T00:00:00Z`) ?? d.day}: ${formatUsd(d.usd)}`}
          style={{ height: max > 0 && d.usd > 0 ? `${Math.max(4, (d.usd / max) * 100)}%` : "2px" }}
        />
      ))}
    </div>
  );
}

/**
 * The money side of an API-key account, in place of the 5-hour and weekly
 * cards a subscription shows: spend this month, day by day, the limits it is
 * measured against, and where the figures come from.
 *
 * Saves go straight to the backend; the snapshot event that follows carries
 * the new figures, so this keeps no copy of them beyond the form fields.
 */
export default function ApiBilling({ account, now, disabled }: { account: Account; now: number; disabled: boolean }) {
  const billing = account.billing;
  const clockFormat = useClockFormat();
  const [limit, setLimit] = useState(inputValue(billing?.monthlyLimitUsd));
  const [balance, setBalance] = useState(inputValue(billing?.prepaidBalanceUsd));
  const [key, setKey] = useState("");
  const [busy, setBusy] = useState<"limits" | "key" | "refresh" | null>(null);
  const [error, setError] = useState<string | null>(null);

  // A different account, or limits saved elsewhere: show what is stored.
  useEffect(() => {
    setLimit(inputValue(billing?.monthlyLimitUsd));
    setBalance(inputValue(billing?.prepaidBalanceUsd));
  }, [account.number, billing?.monthlyLimitUsd, billing?.prepaidBalanceUsd]);

  const run = async (what: "limits" | "key" | "refresh", action: () => Promise<unknown>, fallback: string) => {
    setBusy(what);
    setError(null);
    try {
      await action();
      return true;
    } catch (e) {
      setError(e instanceof Error ? e.message : fallback);
      return false;
    } finally {
      setBusy(null);
    }
  };

  const saveLimits = (e: FormEvent) => {
    e.preventDefault();
    const monthly = parseUsd(limit);
    const prepaid = parseUsd(balance);
    if (monthly === undefined || prepaid === undefined) {
      setError("Enter dollar amounts like 50 or 200.00, or leave a field blank.");
      return;
    }
    void run("limits", () => setBillingLimits(account.number, monthly, prepaid), "Couldn't save the limits.");
  };

  const saveKey = async (e: FormEvent) => {
    e.preventDefault();
    const value = key.trim();
    if (!value) return;
    // Cleared at once, win or lose: the key must not sit in state.
    setKey("");
    await run("key", () => setBillingKey(account.number, value), "Couldn't save the key.");
  };

  const locked = disabled || busy !== null;
  const estimate = billing?.source === "estimate";
  const util = creditUtilisation(account);
  const spentMonth = billing ? formatUsd(billing.monthToDateUsd) : "··";

  return (
    <>
      <div className="limit-card">
        <div className="lc-head">
          <span className="lab">This month{estimate ? " · estimate" : ""}</span>
          <span className="lc-when num">{dayLabel(billing?.resetsAt) ? `resets ${dayLabel(billing?.resetsAt)}` : ""}</span>
        </div>
        <div className="api-amount num">
          {estimate ? "≈ " : ""}
          {spentMonth}
          {billing?.monthlyLimitUsd != null && <small> of {formatUsd(billing.monthlyLimitUsd)}</small>}
        </div>
        {util !== null && <UsageMeter pct={util} />}
        {billing && (
          <span className="lc-foot num">
            Today {formatUsd(billing.todayUsd)} · last 7 days {formatUsd(billing.last7dUsd)}
          </span>
        )}
        {billing && billing.byModel.length > 0 && (
          <ul className="api-models">
            {billing.byModel.slice(0, 3).map((m) => (
              <li key={m.model}>
                <span className="mono">{m.model}</span>
                <span className="num">{formatUsd(m.usd)}</span>
              </li>
            ))}
          </ul>
        )}
      </div>

      {billing && (
        <div className="limit-card">
          <div className="lc-head">
            <span className="lab">Daily spend</span>
            <span className="lc-when num">last {billing.daily.length} days</span>
          </div>
          <DailySpendBars daily={billing.daily} />
        </div>
      )}

      <form className="limit-card" onSubmit={saveLimits}>
        <div className="lc-head">
          <span className="lab">Limits</span>
          {billing?.balanceLeftUsd != null && (
            <span className={`lc-when num${billing.balanceLeftUsd <= 0 ? " danger" : ""}`}>{formatUsd(billing.balanceLeftUsd)} left</span>
          )}
        </div>
        <label className="api-field">
          <span>Monthly limit</span>
          <input
            className="input num"
            inputMode="decimal"
            placeholder="none"
            value={limit}
            onChange={(e) => setLimit(e.target.value)}
            disabled={locked}
            aria-label="Monthly limit in US dollars"
          />
        </label>
        <label className="api-field">
          <span>Credit balance</span>
          <input
            className="input num"
            inputMode="decimal"
            placeholder="not set"
            value={balance}
            onChange={(e) => setBalance(e.target.value)}
            disabled={locked}
            aria-label="Prepaid credit balance in US dollars"
          />
        </label>
        <span className="lc-foot">
          Anthropic doesn&apos;t publish your credit balance to apps. Enter it from the Console&apos;s billing page and
          spend from then on is taken off
          {billing?.balanceSetAt ? ` (entered ${formatClock(billing.balanceSetAt, clockFormat, now) ?? dayLabel(billing.balanceSetAt) ?? ""})` : ""}.
          Auto-switch stops using this account once a limit is reached.
        </span>
        <div>
          <button type="submit" className="btn btn-sm" disabled={locked}>
            {busy === "limits" ? "Saving…" : "Save limits"}
          </button>
        </div>
      </form>

      <div className="limit-card">
        <div className="lc-head">
          <span className="lab">Spend source</span>
          <button
            type="button"
            className="btn ghost btn-sm"
            disabled={locked}
            onClick={() => void run("refresh", refreshBilling, "Couldn't refresh spend.")}
          >
            {busy === "refresh" ? "Refreshing…" : "Refresh"}
          </button>
        </div>
        {billing?.hasAdminKey ? (
          <>
            <span className="lc-foot">
              Exact, from your organisation&apos;s cost report: the same figure as the Console. Read with Admin key
              {billing.adminKeyHint ? <span className="mono"> ••••{billing.adminKeyHint}</span> : null}.
            </span>
            <div>
              <button
                type="button"
                className="btn ghost btn-sm"
                disabled={locked}
                onClick={() => void run("key", () => setBillingKey(account.number, null), "Couldn't remove the key.")}
              >
                Remove key
              </button>
            </div>
          </>
        ) : (
          <form className="api-key-form" onSubmit={(e) => void saveKey(e)}>
            <span className="lc-foot">
              {estimate
                ? "Estimated from Claude Code's logs on this machine, for the time this account was in use. "
                : "Not read yet. "}
              For exact figures that match the Console, add an Admin key (Console → Settings → Admin keys). It is
              used only to read your organisation&apos;s cost report.
            </span>
            <input
              className="input mono"
              type="password"
              autoComplete="off"
              spellCheck={false}
              placeholder="sk-ant-admin01-…"
              value={key}
              onChange={(e) => setKey(e.target.value)}
              disabled={locked}
              aria-label="Admin API key"
            />
            <div>
              <button type="submit" className="btn btn-sm" disabled={locked || key.trim() === ""}>
                {busy === "key" ? "Checking…" : "Save key"}
              </button>
            </div>
          </form>
        )}
        {billing?.fetchedAt && (
          <span className="lc-foot num">Last read {formatClock(billing.fetchedAt, clockFormat, now) ?? "—"}</span>
        )}
        {billing?.error && <span className="lc-foot danger">{billing.error}</span>}
        {error && (
          <span className="lc-foot danger" role="alert">
            {error}
          </span>
        )}
      </div>
    </>
  );
}
