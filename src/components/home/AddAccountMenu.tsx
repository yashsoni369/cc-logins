import { SignInWait } from "../SignInWait";
import MenuButton, { type MenuItem } from "../ui/MenuButton";

interface AddAccountMenuProps {
  /**
   * Whether Claude Code on this machine is signed in to some account.
   * `undefined` is undetermined and reads as "maybe", never as "no".
   */
  loginPresent: boolean | undefined;
  onAddCurrent: () => void;
  onSignIn: () => void;
  /** Label while a route is in flight, e.g. "Adding…". */
  busyLabel: string | null;
  /** All mutations share one credential lock, so every route disables together. */
  disabled: boolean;
}

/**
 * One "Add account" entry point instead of three buttons and a paragraph
 * explaining their difference. The route this machine can already satisfy —
 * adding the login Claude Code is signed into — is listed first and marked.
 */
export default function AddAccountMenu({ loginPresent, onAddCurrent, onSignIn, busyLabel, disabled }: AddAccountMenuProps) {
  const current: MenuItem = {
    id: "current",
    label: (
      <>
        {loginPresent === true && <span className="menu-found">Detected on this machine</span>}
        Use the account Claude Code is signed into
      </>
    ),
    description: "It keeps using Claude Code's own folder. Nothing is copied.",
    onSelect: onAddCurrent,
  };
  const signIn: MenuItem = {
    id: "signin",
    label: "Sign in to another account",
    description: (
      <>
        Opens the official <code>claude auth login</code> in a terminal, into a new folder for that account.
      </>
    ),
    onSelect: onSignIn,
  };
  const items = loginPresent !== false ? [current, signIn] : [signIn, current];

  return (
    <MenuButton buttonClassName="btn primary" items={items} disabled={disabled} menuClassName="menu-wide">
      {busyLabel ?? "Add account"}
      <svg width="10" height="10" viewBox="0 0 16 16" fill="none" aria-hidden="true" className="caret">
        <path d="M4 6l4 4 4-4" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" />
      </svg>
    </MenuButton>
  );
}

/** What adding an account has to say: the sign-in wait and errors. */
export function AddAccountPanel({
  pendingSignIn,
  signInError,
  addCurrentError,
}: {
  pendingSignIn: boolean;
  signInError: string | null;
  addCurrentError: string | null;
}) {
  if (!pendingSignIn && !signInError && !addCurrentError) return null;
  return (
    <div className="add-panel">
      {pendingSignIn && <SignInWait />}
      {!pendingSignIn && signInError && (
        <div className="banner" role="alert">
          <span>{signInError}</span>
        </div>
      )}
      {addCurrentError && (
        <div className="banner danger" role="alert">
          <span>{addCurrentError}</span>
        </div>
      )}
    </div>
  );
}
