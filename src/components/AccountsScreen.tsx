import { Fragment, useState, type CSSProperties, type KeyboardEvent } from "react";
import type { Account, Snapshot } from "../types";
import { ageLabel, displayName, maskEmail } from "../types";
import UsageMeter from "./UsageMeter";
import AccountDetails from "./AccountDetails";
import AddTokenDialog from "./AddTokenDialog";
import SignInFlow from "./SignInFlow";
import { RefreshButton } from "./RefreshButton";
import { formatCountdown, formatInstant, useNow } from "../lib/time";
import { useClockFormat } from "../lib/clockFormat";

/** Per-account outcome of the most recent switch attempt. */
interface SwitchError {
  accountNumber: number;
  message: string;
}

/** Per-account outcome of the most recent enable/disable attempt. */
interface EnableError {
  accountNumber: number;
  message: string;
}

interface ReloginError {
  accountNumber: number;
  message: string;
}

interface AccountsScreenProps {
  snapshot: Snapshot;
  /** Invoked only from this screen's Switch button onClick — never elsewhere. */
  onSwitch: (accountNumber: number) => void;
  /** Account currently mid-switch, if any. Disables every Switch button. */
  pendingAccount: number | null;
  /** Result of the most recent failed switch, if any. */
  switchError: SwitchError | null;

  /** Invoked only from the "Add the account I'm signed into" button onClick. */
  onAddAccount: () => void;
  pendingAddAccount: boolean;
  addAccountError: string | null;

  /** Invoked only from AddTokenDialog's submit — itself only reachable via a click. */
  onAddToken: (token: string, email?: string, alias?: string) => Promise<void>;
  pendingAddToken: boolean;
  addTokenError: string | null;

  /** Invoked only from SignInFlow's own button onClick. */
  onInteractiveLogin: () => void;
  pendingInteractiveLogin: boolean;
  /** `null` covers both rest and a quiet cancellation — SignInFlow renders nothing for either. */
  interactiveLoginError: string | null;

  /** Re-authenticate an existing dead OAuth slot without creating a duplicate. */
  onRelogin: (accountNumber: number) => void;
  pendingReloginAccount: number | null;
  reloginError: ReloginError | null;

  /** Invoked only from a row's Enable/Disable button onClick. */
  onSetEnabled: (accountNumber: number, enabled: boolean) => void;
  pendingEnableAccount: number | null;
  enableError: EnableError | null;

  /**
   * True while any mutating call above is in flight. All mutating buttons
   * disable together — they all touch the same single-writer credential
   * store the backend guards with its "busy" lock, so letting two run at
   * once would just mean racing to hit that lock.
   */
  mutationInFlight: boolean;

  /**
   * True when the most recent background refresh failed. The last-good
   * values are still shown (never blanked), but meters dim so a stale number
   * never reads as freshly confirmed.
   */
  degraded: boolean;
}

/** Freshest (smallest) measurement age across all accounts, if any is known. */
function freshestAgeSeconds(accounts: Account[]): number | undefined {
  const ages = accounts
    .map((a) => a.usageAgeSeconds)
    .filter((s): s is number => s != null);
  return ages.length ? Math.min(...ages) : undefined;
}

/** Header freshness readout, e.g. "measured 4s ago". */
function measuredLabel(accounts: Account[]): string {
  const age = freshestAgeSeconds(accounts);
  if (age == null) return "measured —";
  if (age < 60) return `measured ${Math.round(age)}s ago`;
  const label = ageLabel(age);
  return label ? `measured ${label}` : "measured just now";
}

export default function AccountsScreen({
  snapshot,
  onSwitch,
  pendingAccount,
  switchError,
  onAddAccount,
  pendingAddAccount,
  addAccountError,
  onAddToken,
  pendingAddToken,
  addTokenError,
  onInteractiveLogin,
  pendingInteractiveLogin,
  interactiveLoginError,
  onRelogin,
  pendingReloginAccount,
  reloginError,
  onSetEnabled,
  pendingEnableAccount,
  enableError,
  mutationInFlight,
  degraded,
}: AccountsScreenProps) {
  const accounts = snapshot.environments.flatMap((e) => e.accounts);

  // Countdowns are recomputed here, not read off the snapshot: the backend
  // bakes them at snapshot-build time and only re-polls every ~5 minutes, so
  // its strings drift. The shared ticker keeps every row honest between polls.
  const now = useNow();
  const clockFormat = useClockFormat();

  // Purely local UI state — which row is expanded and whether the token form
  // is open. Neither one touches credentials, so neither needs to live
  // alongside the mutation state in App.
  const [expandedAccount, setExpandedAccount] = useState<number | null>(null);
  const [showAddToken, setShowAddToken] = useState(false);

  return (
    <div className="pane">
      <div className="pane-head">
        <h3>Accounts</h3>
        <span className="sub num">{measuredLabel(accounts)}</span>
        <RefreshButton />
      </div>

      {/*
        Header cells are `nowrap`, so at the 720px minimum window width the
        table's min-content width exceeds the pane. Scrolling it here keeps
        that out of the window. tabindex + role make the scroll region
        keyboard-reachable, which browsers do not do on their own.
      */}
      <div className="table-scroll" tabIndex={0} role="region" aria-label="Accounts table">
      <table className="accts">
        <thead>
          <tr>
            <th style={{ width: "36%" }}>Account</th>
            <th>5-hour</th>
            <th>7-day</th>
            <th className="r">Resets in</th>
            <th></th>
          </tr>
        </thead>
        <tbody>
          {accounts.map((account) => {
            const isHeldOut = account.usageStatus === "disabled";
            const needsRelogin = account.usageStatus === "reloginrequired";
            const hasForeignCredential = account.usageStatus === "foreigncredential";
            const fiveHour = account.usage?.fiveHour;
            // Fallback chain mirrors `fresh_reset_strings` in oauth.rs: when
            // `resetsAt` is missing or unparseable, show the backend's own
            // (drifting) strings rather than nothing — stale beats blank.
            const resets =
              formatCountdown(fiveHour?.resetsAt, now) ?? fiveHour?.countdown ?? fiveHour?.clock ?? "—";
            // The countdown loses the exact instant, so hover keeps it.
            const resetsAtLabel = formatInstant(fiveHour?.resetsAt, clockFormat) ?? undefined;
            const age = ageLabel(account.usageAgeSeconds);
            const meterStyle: CSSProperties | undefined = degraded ? { opacity: 0.55 } : undefined;
            const isExpanded = expandedAccount === account.number;
            const isEnablePending = pendingEnableAccount === account.number;
            // Details only exists for the active account today — it's the
            // only row with anything AccountDetails can't already show via
            // the table's own columns worth expanding for.
            // Every row expands, not just the active one. The details panel
            // shows per-model windows, organisation and exact reset times —
            // all of which are just as relevant for an account you are
            // considering switching TO. A single clickable row among
            // non-clickable neighbours is also worse than none: it makes the
            // whole table read as inert.
            const canExpand = true;

            const toggleExpanded = () => setExpandedAccount(isExpanded ? null : account.number);

            return (
              <Fragment key={account.number}>
                <tr
                  className={[account.active ? "is-active" : null, canExpand ? "acct-row" : null]
                    .filter(Boolean)
                    .join(" ") || undefined}
                  {...(canExpand
                    ? {
                        role: "button" as const,
                        tabIndex: 0,
                        "aria-expanded": isExpanded,
                        onClick: toggleExpanded,
                        onKeyDown: (e: KeyboardEvent<HTMLTableRowElement>) => {
                          if (e.key === "Enter" || e.key === " ") {
                            e.preventDefault();
                            toggleExpanded();
                          }
                        },
                      }
                    : {})}
                >
                  <td>
                    <div className="who">
                      {canExpand && (
                        <span className={`acct-caret${isExpanded ? " is-open" : ""}`} aria-hidden="true">
                          <svg width="10" height="10" viewBox="0 0 16 16" fill="none">
                            <path
                              d="M6 4l4 4-4 4"
                              stroke="currentColor"
                              strokeWidth="1.6"
                              strokeLinecap="round"
                              strokeLinejoin="round"
                            />
                          </svg>
                        </span>
                      )}
                      <span className={`mark${account.active ? " on" : ""}`}></span>
                      <div>
                        <div className="alias" style={isHeldOut ? { color: "var(--faint)" } : undefined}>
                          {displayName(account)}{" "}
                          {account.active && <span className="pill on">active</span>}
                          {isHeldOut && <span className="pill">held out</span>}
                          {hasForeignCredential && <span className="pill danger">credential mismatch</span>}
                          {needsRelogin && <span className="pill danger">Re-login required</span>}
                          {age && <span className="pill">{age}</span>}
                        </div>
                        <div className="mail">{maskEmail(account.email)}</div>
                        {needsRelogin && (
                          <div style={{ marginTop: 3, fontSize: 11, color: "var(--danger)" }}>
                            Sign in again to replace this account&apos;s rejected login.
                          </div>
                        )}
                      </div>
                    </div>
                  </td>
                  <td>
                    <div style={meterStyle}>
                      <UsageMeter pct={account.usage?.fiveHour?.pct} />
                    </div>
                  </td>
                  <td>
                    <div style={meterStyle}>
                      <UsageMeter pct={account.usage?.sevenDay?.pct} />
                    </div>
                  </td>
                  <td
                    className="r num"
                    title={resetsAtLabel}
                    style={{ fontSize: "12px", color: resets === "—" ? "var(--faint)" : "var(--muted)" }}
                  >
                    {resets}
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
                            onRelogin(account.number);
                          }}
                        >
                          {pendingReloginAccount === account.number ? "Signing in…" : "Re-login"}
                        </button>
                      ) : !account.active && !isHeldOut ? (
                        <button
                          type="button"
                          className="btn"
                          disabled={mutationInFlight}
                          onClick={(e) => {
                            e.stopPropagation();
                            onSwitch(account.number);
                          }}
                        >
                          {pendingAccount === account.number ? "Switching…" : "Switch"}
                        </button>
                      ) : null}
                      {/*
                        A labelled button, not a switch: it sits beside Switch
                        and Re-login, and a lone toggle among buttons read as
                        an odd one out. `stopPropagation` is still required —
                        the row is itself a click-to-expand button.
                      */}
                      <button
                        type="button"
                        className="btn ghost"
                        disabled={mutationInFlight}
                        title={
                          isHeldOut
                            ? "Held out of rotation. Enable to let auto-switch use it."
                            : "In auto-switch rotation. Disable to hold it out."
                        }
                        onClick={(e) => {
                          e.stopPropagation();
                          onSetEnabled(account.number, isHeldOut);
                        }}
                      >
                        {isEnablePending
                          ? isHeldOut
                            ? "Enabling…"
                            : "Disabling…"
                          : isHeldOut
                            ? "Enable"
                            : "Disable"}
                      </button>
                    </div>
                    {switchError?.accountNumber === account.number && (
                      <div style={{ marginTop: 6, fontSize: 11, color: "var(--danger)", textAlign: "right" }}>
                        {switchError.message}
                      </div>
                    )}
                    {enableError?.accountNumber === account.number && (
                      <div style={{ marginTop: 6, fontSize: 11, color: "var(--danger)", textAlign: "right" }}>
                        {enableError.message}
                      </div>
                    )}
                    {reloginError?.accountNumber === account.number && (
                      <div style={{ marginTop: 6, fontSize: 11, color: "var(--danger)", textAlign: "right" }}>
                        {reloginError.message}
                      </div>
                    )}
                  </td>
                </tr>
                {isExpanded && (
                  <tr className="acct-details-row">
                    <td colSpan={5}>
                      <AccountDetails account={account} />
                    </td>
                  </tr>
                )}
              </Fragment>
            );
          })}
        </tbody>
      </table>
      </div>

      <hr className="rule" />

      <div className="add-row">
        <SignInFlow
          pending={pendingInteractiveLogin}
          error={interactiveLoginError}
          disabled={mutationInFlight && !pendingInteractiveLogin}
          onStart={onInteractiveLogin}
        />

        <div className="add-row-actions">
          <button
            type="button"
            className="btn"
            disabled={mutationInFlight}
            onClick={onAddAccount}
          >
            {pendingAddAccount ? "Adding…" : "Add the account I'm signed into"}
          </button>
          <button
            type="button"
            className="btn"
            disabled={mutationInFlight}
            onClick={() => setShowAddToken((v) => !v)}
          >
            Add token
          </button>
        </div>
        <p className="add-hint">
          <b>Add the account I&apos;m signed into</b> registers whichever Claude Code login is currently active on
          this machine — it does not open a new sign-in. If you want to add a different login, sign in with Claude
          Code first, then press this.
        </p>
        {addAccountError && (
          <div className="banner danger" role="alert">
            <span>{addAccountError}</span>
          </div>
        )}
        {showAddToken && (
          <AddTokenDialog
            pending={pendingAddToken}
            error={addTokenError}
            onCancel={() => setShowAddToken(false)}
            onSubmit={async (token, email, alias) => {
              await onAddToken(token, email, alias);
              setShowAddToken(false);
            }}
          />
        )}
      </div>
    </div>
  );
}
