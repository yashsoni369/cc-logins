import Dialog from "./ui/Dialog";

/**
 * Home banner: accounts have their own folders, but the `claude` command that
 * starts new sessions on the picked account is missing or shadowed. Without
 * it, picking an account changes nothing for plain `claude`.
 */
export function ClaudeCommandBanner({
  shadowedBy,
  installing,
  error,
  onInstall,
  onOpenSettings,
}: {
  shadowedBy: string | null;
  installing: boolean;
  error: string | null;
  onInstall: () => void;
  onOpenSettings: () => void;
}) {
  if (shadowedBy) {
    return (
      <div className="banner" role="status">
        <span>
          Another <code>claude</code> ({shadowedBy}) comes before CC Logins&apos; on your PATH, so new terminals won&apos;t
          use the account you pick.
        </span>
        <button type="button" className="btn ghost btn-sm" onClick={onOpenSettings}>
          How to fix
        </button>
      </div>
    );
  }
  return (
    <div className="banner" role="status">
      <span>
        Install the <code>claude</code> command so new terminals start the account you pick. Until then, plain{" "}
        <code>claude</code> always uses your default account.
        {error && <span className="field-error"> {error}</span>}
      </span>
      <button type="button" className="btn primary btn-sm" disabled={installing} onClick={onInstall}>
        {installing ? "Installing…" : "Install"}
      </button>
    </div>
  );
}

/** Shown when the user picks an account before the command is installed. */
export function ClaudeCommandDialog({
  open,
  accountName,
  installing,
  error,
  onInstallAndUse,
  onCancel,
}: {
  open: boolean;
  accountName: string;
  installing: boolean;
  error: string | null;
  onInstallAndUse: () => void;
  onCancel: () => void;
}) {
  return (
    <Dialog open={open} onClose={onCancel} label="Install the claude command first">
      <div className="dlg-body">
        <h2 className="dlg-title">
          Install the <code>claude</code> command first
        </h2>
        <p>
          New sessions use {accountName} through CC Logins&apos; <code>claude</code> command. It puts that command
          first on your PATH; sessions already running keep their account. You can remove it in Settings.
        </p>
        {error && (
          <p className="field-error" role="alert">
            {error}
          </p>
        )}
        <div className="confirm-actions">
          <button type="button" className="btn" onClick={onCancel}>
            Not now
          </button>
          <button type="button" className="btn primary" autoFocus disabled={installing} onClick={onInstallAndUse}>
            {installing ? "Installing…" : `Install and use ${accountName}`}
          </button>
        </div>
      </div>
    </Dialog>
  );
}
