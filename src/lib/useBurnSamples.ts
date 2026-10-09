import { useCallback, useEffect, useMemo, useState } from "react";

import { historySamples } from "@/lib/api";
import { stableKey, type Account, type Sample } from "@/types";

/** Trailing window the burn estimate is measured over — the span the old Dashboard used. */
export const BURN_HOURS = 24;

export interface BurnSamples {
  keyFor: (account: Account) => string | undefined;
  burnByKey: Map<string, Sample[]>;
  /** Samples for the account in use; empty until they load. */
  activeSamples: Sample[];
}

/**
 * Recent recorded history for every account, for the forward-looking
 * readouts (runway, projected limit, coverage).
 *
 * Reads the local history database only — never Anthropic — and re-reads
 * when `refreshKey` changes, so each new snapshot brings the burn rate with
 * it. One account's unreadable history becomes an empty series for that
 * account alone, never a blank screen.
 */
export function useBurnSamples(accounts: Account[], refreshKey: unknown): BurnSamples {
  const [keyByNumber, setKeyByNumber] = useState<Map<number, string>>(new Map());
  const [burnByKey, setBurnByKey] = useState<Map<string, Sample[]>>(new Map());

  // Keyed on identity rather than the array, which is rebuilt on every snapshot.
  const identity = accounts.map((a) => `${a.number}:${a.email}:${a.organizationUuid ?? ""}`).join("|");

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      const pairs = await Promise.all(
        accounts.map(async (account) => [account.number, await stableKey(account)] as const),
      );
      if (!cancelled) setKeyByNumber(new Map(pairs));
    })();
    return () => {
      cancelled = true;
    };
    // `accounts` is read through `identity` on purpose.
  }, [identity]);

  useEffect(() => {
    const keys = [...keyByNumber.values()];
    if (keys.length === 0) return;
    let cancelled = false;
    void (async () => {
      const entries = await Promise.all(
        keys.map(async (key) => {
          try {
            return [key, (await historySamples(key, BURN_HOURS)).data] as const;
          } catch {
            return [key, [] as Sample[]] as const;
          }
        }),
      );
      if (!cancelled) setBurnByKey(new Map(entries));
    })();
    return () => {
      cancelled = true;
    };
  }, [keyByNumber, refreshKey]);

  const keyFor = useCallback((account: Account) => keyByNumber.get(account.number), [keyByNumber]);
  const active = accounts.find((a) => a.active);
  const activeKey = active ? keyByNumber.get(active.number) : undefined;
  const activeSamples = useMemo(() => (activeKey ? (burnByKey.get(activeKey) ?? []) : []), [activeKey, burnByKey]);

  return { keyFor, burnByKey, activeSamples };
}
