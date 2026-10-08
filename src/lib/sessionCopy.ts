/**
 * Wording for the two ways an account can be "the current one".
 *
 * A v0.4 profile account is *selected for new sessions*: picking it changes
 * what the next `claude` starts with, and sessions already running keep their
 * account. A v0.3 account (not yet moved to its own folder) is still swapped
 * in, so it is *in use* everywhere at once. The UI must never describe the
 * first as if it were the second.
 *
 * With the opt-in live swap (Settings -> Switch running sessions too), a
 * profile account is swapped in as well, so it reads like the second.
 */
import type { Account } from "@/types";

let liveSwap = false;

/** Mirror of the `switchRunningSessions` setting; set by `useSettings`. */
export function setLiveSwap(on: boolean): void {
  liveSwap = on;
}

export function isProfile(account: Account): boolean {
  return account.profile !== undefined;
}

/** Picking this account changes only what new sessions start with. */
function newSessionsOnly(account: Account): boolean {
  return isProfile(account) && !liveSwap;
}

/** Pill beside the current account's name. */
export function currentLabel(account: Account): string {
  return newSessionsOnly(account) ? "new sessions" : "in use";
}

/** Tooltip for that pill. */
export function currentTitle(account: Account): string {
  return newSessionsOnly(account)
    ? "New Claude sessions start with this account. Sessions already running keep theirs."
    : "Claude Code is using this account now.";
}

/** Short action button label. */
export function useLabel(account: Account, pending: boolean): string {
  if (newSessionsOnly(account)) return pending ? "Selecting…" : "Use";
  return pending ? "Switching…" : "Switch";
}

/** Long action label, for the drawer. */
export function useLongLabel(account: Account, name: string): string {
  return newSessionsOnly(account) ? "Use for new sessions" : `Switch to ${name}`;
}

/** Confirmation after the user picked an account. */
export function pickedMessage(account: Account, name: string): string {
  return newSessionsOnly(account)
    ? `New sessions will use ${name}. Running sessions keep their account.`
    : liveSwap && isProfile(account)
      ? `Switched to ${name}. Running sessions use it on their next request.`
      : `Switched to ${name}`;
}

/** Notification after auto-switch picked an account. */
export function autoPickedNotice(account: Account | undefined, name: string, from: string): { title: string; body: string } {
  if (account && newSessionsOnly(account)) {
    return {
      title: `New sessions will use ${name}`,
      body: `${from} was near its limit. Start a new claude session to continue on ${name}; running sessions keep their account.`,
    };
  }
  return {
    title: `Switched to ${name}`,
    body: `${from} was near its limit. Claude Code uses the new account on its next request.`,
  };
}

/**
 * How old a profile account's reading is, when it is not live: nobody is
 * using the account, so the reading still holds until a window resets.
 * `null` when there is nothing to say.
 */
export function freshnessLabel(account: Account, formatAge: (seconds: number) => string | null): string | null {
  const freshness = account.usageFreshness;
  if (!freshness || freshness.kind === "live") return null;
  const age = formatAge(freshness.ageSeconds);
  if (freshness.kind === "reset") return age ? `reset since · ${age}` : "reset since";
  return age ? `idle · ${age}` : "idle";
}

/**
 * A v0.3 account: its login is a stored copy. It moves by signing in once
 * into its own folder; until then it cannot be used for new sessions.
 */
export function needsMove(account: Account): boolean {
  return !isProfile(account);
}

export const MOVE_TITLE =
  "Sign in once so this account gets its own Claude Code folder. Its stored copy is deleted after the sign-in is confirmed.";

/** Removing is refused only for a v0.3 account that is live in Claude Code. */
export function removeBlocked(account: Account): boolean {
  return account.active && !isProfile(account);
}

/** Badge for a profile account that cannot start sessions right now. */
export function profileProblem(account: Account): string | null {
  switch (account.profile?.state) {
    case "loginRequired":
      return "Signed out";
    case "identityMismatch":
      return "Different account in folder";
    case "migrationPending":
      return "Sign in to move";
    default:
      return null;
  }
}
