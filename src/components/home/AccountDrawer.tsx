import { useState } from "react";

import PlanBadge from "../PlanBadge";
import Toggle from "../Toggle";
import UsageMeter from "../UsageMeter";
import Dialog from "../ui/Dialog";
import { useClockFormat } from "../../lib/clockFormat";
import { weeklyPaceMark, type Projection } from "../../lib/coverage";
import { formatClock, formatWhen } from "../../lib/time";
import { currentLabel, MOVE_TITLE, needsMove, removeBlocked, useLongLabel } from "../../lib/sessionCopy";
import { ageLabel, displayName, formatSpend, isEnterprise, maskEmail, type Account } from "../../types";

export type DrawerIntent = "view" | "rename" | "remove";

interface AccountDrawerProps {
  account: Account | null;
  intent: DrawerIntent;
  now: number;
  projection: Projection | null;
  onClose: () => void;
  onSwitch: (accountNumber: number) => void;
  onRelogin: (accountNumber: number) => void;
  onSetEnabled: (accountNumber: number, enabled: boolean) => void;
  /** Resolves on success; rejects with an Error whose message is shown inline. */
  onRename: (accountNumber: number, alias: string | null) => Promise<void>;
  onRemove: (accountNumber: number) => Promise<void>;
  onViewHistory: (accountNumber: number) => void;
  mutationInFlight: boolean;
}

const MAX_NAME = 40;

/**
 * Everything about one account, opened from any row: its limits with both
 * countdown and clock, per-model windows, and every action — including the
 * two the old screens lacked, rename and remove.
 *
 * A native `<dialog>` (see `ui/Dialog`) gives focus management and Escape.
 * Removal is confirmed inside the drawer, focuses the safe choice first, and
 * is refused for the account in use (the backend enforces that too).
 */
export default function AccountDrawer(props: AccountDrawerProps) {
  const { account, intent, onClose } = props;
  return (
    <Dialog open={account !== null} onClose={onClose} label={account ? `${displayName(account)} details` : "Account details"} variant="drawer">
      {/* Keyed so opening another account, or the same one for another reason,
          starts from fresh local state — no effect needed to reset it. */}
      {account && <DrawerBody key={`${account.number}:${intent}`} {...props} account={account} />}
    </Dialog>
  );
}

function DrawerBody({
  account,
  intent,
  now,
  projection,
  mutationInFlight,
  onClose,
  onSwitch,
  onRelogin,
  onSetEnabled,
  onRename,
  onRemove,
  onViewHistory,
}: AccountDrawerProps & { account: Account }) {
  const clockFormat = useClockFormat();
  const [editing, setEditing] = useState(intent === "rename");
  const [confirmRemove, setConfirmRemove] = useState(intent === "remove");
  const [draft, setDraft] = useState(account.alias ?? "");
  const [error, setError] = useState<string | null>(null);

  const usage = account.usage;
  const heldOut = account.usageStatus === "disabled";
  const needsRelogin = account.usageStatus === "reloginrequired";
  const age = ageLabel(account.usageAgeSeconds);
  const when = (iso: string | undefined) => formatWhen(iso, clockFormat, now);
  const scoped = usage?.scoped ?? [];

  const cancelRename = () => {
    setEditing(false);
    setDraft(account.alias ?? "");
    setError(null);
  };

  const saveName = async () => {
    const next = draft.trim();
    if (next === (account.alias ?? "")) {
      setEditing(false);
      return;
    }
    if (next.length > MAX_NAME) {
      setError(`Names can be at most ${MAX_NAME} characters.`);
      return;
    }
    try {
      await onRename(account.number, next === "" ? null : next);
      setEditing(false);
      setError(null);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Couldn't rename this account.");
    }
  };

  const remove = async () => {
    try {
      await onRemove(account.number);
    } catch (e) {
      setError(e instanceof Error ? e.message : "Couldn't remove this account.");
      setConfirmRemove(false);
    }
  };

  return (
    <div className="adrawer">
      <header className="adrawer-head">
        <span className={`mark${account.active ? " on" : ""}`} />
        <div className="adrawer-title">
          {editing ? (
            <form
              className="rename-form"
              onSubmit={(e) => {
                e.preventDefault();
                void saveName();
              }}
            >
              <input
                className="input"
                aria-label="Account name"
                autoFocus
                maxLength={MAX_NAME}
                placeholder={maskEmail(account.email)}
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                onKeyDown={(e) => {
                  if (e.key === "Escape") {
                    e.preventDefault();
                    e.stopPropagation();
                    cancelRename();
                  }
                }}
                disabled={mutationInFlight}
              />
              <button type="submit" className="btn primary" disabled={mutationInFlight}>
                Save
              </button>
              <button type="button" className="btn ghost" onClick={cancelRename}>
                Cancel
              </button>
            </form>
          ) : (
            <div className="adrawer-name">
              <h3>
                {displayName(account)} <PlanBadge usage={usage} />
              </h3>
              <button type="button" className="btn ghost btn-sm" onClick={() => setEditing(true)} disabled={mutationInFlight}>
                Rename
              </button>
            </div>
          )}
          <div className="adrawer-sub">
            {maskEmail(account.email)}
            {account.organizationName ? ` · ${account.organizationName}` : ""}
            {account.active ? ` · ${currentLabel(account)}` : ""}
          </div>
        </div>
        <button type="button" className="btn ghost btn-icon" aria-label="Close" onClick={onClose}>
          ✕
        </button>
      </header>

      {error && (
        <div className="banner danger" role="alert">
          <span>{error}</span>
        </div>
      )}

      <div className="adrawer-body">
        {isEnterprise(usage) && usage?.spend ? (
          <div className="limit-card">
            <div className="lc-head">
              <span className="lab">Monthly spend cap</span>
              <span className="lc-when num">{when(usage.spend.resetsAt) ?? "reset unknown"}</span>
            </div>
            <UsageMeter pct={usage.spend.pct} />
            <span className="lc-foot num">{formatSpend(usage.spend)}</span>
          </div>
        ) : (
          <>
            <div className="limit-card">
              <div className="lc-head">
                <span className="lab">5-hour session</span>
                <span className="lc-when num">
                  {usage?.fiveHour ? (when(usage.fiveHour.resetsAt) ?? usage.fiveHour.countdown ?? "reset unknown") : "not reported"}
                </span>
              </div>
              <UsageMeter pct={usage?.fiveHour?.pct} />
              {account.active && projection?.at != null && projection.beforeReset && (
                <span className="lc-foot caution num">
                  At this pace it runs out ~{formatClock(new Date(projection.at).toISOString(), clockFormat, now)}, before it resets.
                </span>
              )}
            </div>
            <div className="limit-card">
              <div className="lc-head">
                <span className="lab">Weekly</span>
                <span className="lc-when num">
                  {usage?.sevenDay ? (when(usage.sevenDay.resetsAt) ?? usage.sevenDay.countdown ?? "reset unknown") : "not reported"}
                </span>
              </div>
              <UsageMeter pct={usage?.sevenDay?.pct} pace={weeklyPaceMark(usage, now)} />
              {usage?.sevenDay?.aheadOfPace != null && (
                <span className={`lc-foot num${usage.sevenDay.aheadOfPace ? " caution" : ""}`}>
                  {usage.sevenDay.aheadOfPace
                    ? usage.sevenDay.willLastToReset === false
                      ? "Ahead of pace. At this rate it runs out before the week resets."
                      : "Ahead of pace, but projected to last until the reset."
                    : "On pace for the week."}
                </span>
              )}
            </div>
          </>
        )}

        {scoped.length > 0 && (
          <div className="limit-card">
            <div className="lc-head">
              <span className="lab">Per-model weekly limits</span>
            </div>
            {scoped.map((s) => (
              <div key={s.name} className="lc-model">
                <span className="lc-model-name">{s.name}</span>
                <UsageMeter pct={s.pct} />
                {when(s.resetsAt) && <span className="lc-when num">{when(s.resetsAt)}</span>}
              </div>
            ))}
          </div>
        )}

        <dl className="adrawer-facts">
          <dt>Measured</dt>
          <dd className="num">
            {account.usageFetchedAt ? (formatClock(account.usageFetchedAt, clockFormat, now) ?? "—") : "never"}
            {age ? ` · ${age}` : ""}
          </dd>
          <dt>Available to auto-switch</dt>
          <dd>
            <Toggle
              checked={!heldOut}
              ariaLabel="Available to auto-switch"
              disabled={mutationInFlight || (account.active && !heldOut)}
              title={account.active && !heldOut ? "The account in use always stays available." : undefined}
              onChange={(next) => onSetEnabled(account.number, next)}
            />
          </dd>
        </dl>
      </div>

      <footer className="adrawer-foot">
        {confirmRemove ? (
          <div className="confirm-remove" role="group" aria-label="Confirm removal">
            <p>
              {needsMove(account) ? (
                <>
                  Remove <b>{displayName(account)}</b> from CC Logins? Its stored login is deleted from this app&apos;s
                  store. Claude Code&apos;s own login is not touched, and you can add the account again later.
                </>
              ) : (
                <>
                  Remove <b>{displayName(account)}</b> from CC Logins? Its Claude Code folder, with its sign-in and
                  session history, stays on disk. You can add the account again later.
                </>
              )}
            </p>
            <div className="confirm-actions">
              <button type="button" className="btn" autoFocus onClick={() => setConfirmRemove(false)}>
                Keep it
              </button>
              <button type="button" className="btn danger" disabled={mutationInFlight || removeBlocked(account)} onClick={() => void remove()}>
                Remove account
              </button>
            </div>
          </div>
        ) : (
          <>
            {needsRelogin ? (
              <button type="button" className="btn primary" disabled={mutationInFlight} onClick={() => onRelogin(account.number)}>
                Re-sign in
              </button>
            ) : needsMove(account) ? (
              <button type="button" className="btn primary" title={MOVE_TITLE} disabled={mutationInFlight} onClick={() => onRelogin(account.number)}>
                Sign in to move
              </button>
            ) : (
              !account.active &&
              !heldOut && (
                <button type="button" className="btn primary" disabled={mutationInFlight} onClick={() => onSwitch(account.number)}>
                  {useLongLabel(account, displayName(account))}
                </button>
              )
            )}
            <button type="button" className="btn" onClick={() => onViewHistory(account.number)}>
              View history
            </button>
            <span className="sp" />
            <button
              type="button"
              className="btn ghost danger-text"
              disabled={mutationInFlight || removeBlocked(account)}
              title={removeBlocked(account) ? "Switch to another account before removing this one." : undefined}
              onClick={() => setConfirmRemove(true)}
            >
              Remove…
            </button>
          </>
        )}
      </footer>
    </div>
  );
}
