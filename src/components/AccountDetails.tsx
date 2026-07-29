import { useClockFormat } from "../lib/clockFormat";
import { formatInstant } from "../lib/time";
import type { Account } from "../types";
import { ageLabel } from "../types";
import UsageMeter from "./UsageMeter";

interface AccountDetailsProps {
  account: Account;
}

/**
 * What the table's columns can't show: per-model scoped windows, the
 * organisation this login belongs to, exact (not relative) reset instants,
 * and how old the measurement is. Rendered in place under the account's row
 * — no modal, no navigation — so on a single-account machine, where this is
 * the only button that does anything, pressing it is still worth it.
 */
export default function AccountDetails({ account }: AccountDetailsProps) {
  const usage = account.usage;
  const scoped = usage?.scoped ?? [];
  const age = ageLabel(account.usageAgeSeconds);
  const clockFormat = useClockFormat();
  // Absolute instants throughout — countdowns belong to the popover. `"—"` when
  // unknown; a time is never fabricated.
  const instant = (iso: string | undefined) => formatInstant(iso, clockFormat) ?? "—";

  return (
    <div className="acct-details">
      <div className="acct-details-grid">
        <div className="acct-details-field">
          <span className="lab">Organisation</span>
          <span className="val">{account.organizationName ?? "—"}</span>
        </div>
        <div className="acct-details-field">
          <span className="lab">5-hour resets</span>
          <span className="val num">{instant(usage?.fiveHour?.resetsAt)}</span>
        </div>
        <div className="acct-details-field">
          <span className="lab">7-day resets</span>
          <span className="val num">{instant(usage?.sevenDay?.resetsAt)}</span>
        </div>
        <div className="acct-details-field">
          <span className="lab">Measured</span>
          <span className="val num">
            {instant(account.usageFetchedAt)}
            {age ? ` · ${age}` : ""}
          </span>
        </div>
      </div>

      <div className="acct-details-models">
        <div className="acct-details-sub">Per-model weekly windows</div>
        {scoped.length === 0 ? (
          <span style={{ fontSize: 12, color: "var(--faint)" }}>None reported.</span>
        ) : (
          scoped.map((s) => (
            <div key={s.name} className="acct-details-model-row">
              <span className="acct-details-model-name">{s.name}</span>
              <UsageMeter pct={s.pct} />
              <span className="acct-details-reset num">resets {instant(s.resetsAt)}</span>
            </div>
          ))
        )}
      </div>
    </div>
  );
}
