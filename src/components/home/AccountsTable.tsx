import type { CSSProperties, KeyboardEvent } from "react";

import PlanBadge from "../PlanBadge";
import Toggle from "../Toggle";
import UsageMeter from "../UsageMeter";
import MenuButton from "../ui/MenuButton";
import { useClockFormat } from "../../lib/clockFormat";
import { weeklyPaceMark, type Projection } from "../../lib/coverage";
import { formatClock, formatCountdown } from "../../lib/time";
import {
  ageLabel,
  apiSpendLine,
  bindingUtilisation,
  bindingWindow,
  creditUtilisation,
  displayName,
  formatSpend,
  isApiAccount,
  isEnterprise,
  maskEmail,
  type Account,
} from "../../types";

export interface RowError {
  accountNumber: number;
  message: string;
}

export interface AccountsTableProps {
  /** In the backend's rotation order. */
  accounts: Account[];
  now: number;
  /** Projected limit for the account in use, keyed by slot. */
  projections: Map<number, Projection>;
  /** Slot auto-switch would pick next, marked "best next". */
  bestNext: number | null;
  onOpen: (accountNumber: number) => void;
  onSwitch: (accountNumber: number) => void;
  pendingAccount: number | null;
  switchError: RowError | null;
  onRelogin: (accountNumber: number) => void;
  pendingReloginAccount: number | null;
  reloginError: RowError | null;
  onSetEnabled: (accountNumber: number, enabled: boolean) => void;
  pendingEnableAccount: number | null;
  enableError: RowError | null;
  onMove: (accountNumber: number, direction: -1 | 1) => void;
  onRename: (accountNumber: number) => void;
  onRemove: (accountNumber: number) => void;
  /** All mutating controls disable together: they share one credential lock. */
  mutationInFlight: boolean;
  /** The most recent background refresh failed: meters dim, values stay. */
  degraded: boolean;
}

/** Which window gates the account, as the chip label. */
function limitLabel(account: Account): string | null {
  if (isApiAccount(account)) return "API";
  const usage = account.usage;
  if (!usage) return null;
  if (isEnterprise(usage)) return "$ cap";
  const binding = bindingWindow(usage);
  if (!binding) return null;
  if (binding === usage.fiveHour) return "5h";
  if (binding === usage.sevenDay) return "7d";
  if (binding === usage.spend) return "$ cap";
  return usage.scoped?.find((s) => s === binding)?.name ?? null;
}

/**
 * Every account in one table: which window limits it, how full that window
 * is (with a pace tick when the weekly window is the limit), when the account
 * in use is projected to run out, and when its limiting window resets.
 *
 * The reset is always the *limiting* window's — pairing a percentage with
 * another window's clock is worse than showing neither.
 */
export default function AccountsTable(props: AccountsTableProps) {
  const { accounts, now, projections, bestNext, mutationInFlight, degraded } = props;
  const clockFormat = useClockFormat();
  const meterStyle: CSSProperties | undefined = degraded ? { opacity: 0.55 } : undefined;

  return (
    <div className="table-scroll" tabIndex={0} role="region" aria-label="Accounts table">
      <table className="accts home-accts">
        <thead>
          <tr>
            <th style={{ width: "30%" }}>Account</th>
            <th>Limit</th>
            <th>Used</th>
            <th className="r">Runs out</th>
            <th className="r">Resets in</th>
            <th className="c">Auto</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {accounts.map((account, index) => {
            const heldOut = account.usageStatus === "disabled";
            const needsRelogin = account.usageStatus === "reloginrequired";
            const mismatch = account.usageStatus === "foreigncredential";
            const api = isApiAccount(account);
            const binding = bindingWindow(account.usage);
            // An API account has no quota windows; its one reset is the
            // monthly spend figure starting over.
            const resetsAt = api ? account.billing?.resetsAt : binding?.resetsAt;
            // Recomputed from `resetsAt`; the backend's own strings are the
            // fallback — stale beats blank, as in `fresh_reset_strings`.
            const resets =
              formatCountdown(resetsAt, now) ?? (api ? undefined : (binding?.countdown ?? binding?.clock)) ?? "—";
            const resetsTitle = formatClock(resetsAt, clockFormat, now) ?? undefined;
            const age = ageLabel(account.usageAgeSeconds);
            const limit = limitLabel(account);
            const util = api ? creditUtilisation(account) : bindingUtilisation(account.usage);
            const pace = binding && binding === account.usage?.sevenDay ? weeklyPaceMark(account.usage, now) : null;
            const spend = isEnterprise(account.usage) ? account.usage?.spend : undefined;
            const isBest = bestNext === account.number;
            const projection = projections.get(account.number);

            let runsOut = { text: "—", tone: "faint", title: undefined as string | undefined };
            if (util !== null && util >= 100) {
              runsOut = api
                ? { text: "no credit", tone: "danger", title: "A money limit set for this account is used up." }
                : { text: "at limit", tone: "danger", title: undefined };
            } else if (account.active && projection?.at != null) {
              runsOut = projection.beforeReset
                ? {
                    text: formatClock(new Date(projection.at).toISOString(), clockFormat, now) ?? "—",
                    tone: "danger",
                    title: "Projected from the last few hours of use — an estimate.",
                  }
                : { text: "lasts", tone: "muted", title: "At this pace the window resets before it fills." };
            }

            const open = () => props.onOpen(account.number);

            return (
              <tr
                key={account.number}
                className={`acct-row${account.active ? " is-active" : ""}`}
                role="button"
                tabIndex={0}
                aria-label={`${displayName(account)} — details`}
                onClick={open}
                onKeyDown={(e: KeyboardEvent<HTMLTableRowElement>) => {
                  if (e.target !== e.currentTarget) return;
                  if (e.key === "Enter" || e.key === " ") {
                    e.preventDefault();
                    open();
                  }
                }}
              >
                <td>
                  <div className="who">
                    <span className={`mark${account.active ? " on" : ""}`}></span>
                    <div style={{ minWidth: 0 }}>
                      <div className="alias" style={heldOut ? { color: "var(--faint)" } : undefined}>
                        {displayName(account)} <PlanBadge usage={account.usage} apiKey={api} />{" "}
                        {account.active && <span className="pill on">in use</span>}
                        {isBest && <span className="pill best">best next</span>}
                        {heldOut && <span className="pill">held out</span>}
                        {mismatch && <span className="pill danger">credential mismatch</span>}
                        {needsRelogin && <span className="pill danger">Re-login required</span>}
                        {age && <span className="pill">{age}</span>}
                      </div>
                      <div className="mail">{maskEmail(account.email)}</div>
                      {needsRelogin && (
                        <div className="row-hint danger">Sign in again to replace this account&apos;s rejected login.</div>
                      )}
                    </div>
                  </div>
                </td>
                <td>{limit ? <span className="limit-chip">{limit}</span> : <span className="faint">—</span>}</td>
                <td>
                  <div className="used-cell" style={meterStyle}>
                    <UsageMeter pct={util} pace={pace} />
                    {api ? (
                      <span className="used-sub num">{apiSpendLine(account.billing)}</span>
                    ) : spend ? (
                      <span className="used-sub num">{formatSpend(spend)} · monthly</span>
                    ) : account.usage?.fiveHour && account.usage?.sevenDay ? (
                      <span className="used-sub num">
                        5h {Math.round(account.usage.fiveHour.pct)}% · 7d {Math.round(account.usage.sevenDay.pct)}%
                      </span>
                    ) : null}
                  </div>
                </td>
                <td className={`r num runs-out ${runsOut.tone}`} title={runsOut.title}>
                  {runsOut.text}
                </td>
                <td className={`r num resets${resets === "—" ? " faint" : ""}`} title={resetsTitle}>
                  {resets}
                </td>
                <td className="c">
                  <Toggle
                    checked={!heldOut}
                    ariaLabel={`${displayName(account)} available to auto-switch`}
                    title={
                      account.active && !heldOut
                        ? "The account in use always stays available."
                        : heldOut
                          ? "Held out: auto-switch never picks it."
                          : "Auto-switch may pick this account."
                    }
                    disabled={mutationInFlight || (account.active && !heldOut)}
                    pending={props.pendingEnableAccount === account.number}
                    stopPropagation
                    onChange={(next) => props.onSetEnabled(account.number, next)}
                  />
                </td>
                <td className="r">
                  <div className="acct-actions">
                    {needsRelogin ? (
                      <button
                        type="button"
                        className="btn primary"
                        disabled={mutationInFlight}
                        onClick={(e) => {
                          e.stopPropagation();
                          props.onRelogin(account.number);
                        }}
                        onKeyDown={(e) => e.stopPropagation()}
                      >
                        {props.pendingReloginAccount === account.number ? "Signing in…" : "Re-login"}
                      </button>
                    ) : !account.active && !heldOut ? (
                      <button
                        type="button"
                        className={`btn${isBest ? " primary" : ""}`}
                        disabled={mutationInFlight}
                        onClick={(e) => {
                          e.stopPropagation();
                          props.onSwitch(account.number);
                        }}
                        onKeyDown={(e) => e.stopPropagation()}
                      >
                        {props.pendingAccount === account.number ? "Switching…" : "Switch"}
                      </button>
                    ) : null}
                    <MenuButton
                      buttonClassName="btn ghost btn-icon"
                      ariaLabel={`More actions for ${displayName(account)}`}
                      items={[
                        { id: "details", label: "Details", onSelect: open },
                        { id: "rename", label: "Rename…", disabled: mutationInFlight, onSelect: () => props.onRename(account.number) },
                        { id: "up", label: "Move up", disabled: mutationInFlight || index === 0, onSelect: () => props.onMove(account.number, -1) },
                        {
                          id: "down",
                          label: "Move down",
                          disabled: mutationInFlight || index === accounts.length - 1,
                          onSelect: () => props.onMove(account.number, 1),
                        },
                        {
                          id: "remove",
                          label: "Remove…",
                          tone: "danger",
                          disabled: mutationInFlight || account.active,
                          title: account.active ? "Switch to another account before removing this one." : undefined,
                          onSelect: () => props.onRemove(account.number),
                        },
                      ]}
                    >
                      <span aria-hidden="true">⋯</span>
                    </MenuButton>
                  </div>
                  {[props.switchError, props.enableError, props.reloginError].map((err, i) =>
                    err?.accountNumber === account.number ? (
                      <div key={i} className="row-error" role="alert">
                        {err.message}
                      </div>
                    ) : null,
                  )}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}
