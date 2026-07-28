import { useState, type FormEvent } from "react";

interface AddTokenDialogProps {
  pending: boolean;
  /** Message from the most recent failed submit, if any. Owned by the caller. */
  error: string | null;
  /** Registers the token. Rejects on failure — this component never formats the error itself. */
  onSubmit: (token: string, email?: string, alias?: string) => Promise<void>;
  onCancel: () => void;
}

/**
 * Small inline form for registering an account by setup-token or API key,
 * rendered in place under the Accounts table — not a modal.
 *
 * The token never leaves this component except as an argument to `onSubmit`:
 * it is not logged, not echoed anywhere else in the UI, and is cleared from
 * state the instant submit fires, win or lose, so it does not sit around in
 * memory (or React devtools) any longer than it has to.
 */
export default function AddTokenDialog({ pending, error, onSubmit, onCancel }: AddTokenDialogProps) {
  const [token, setToken] = useState("");
  const [email, setEmail] = useState("");
  const [alias, setAlias] = useState("");

  const handleSubmit = (e: FormEvent) => {
    e.preventDefault();
    const value = token.trim();
    if (!value) return;
    // Clear immediately — the token must not linger in state one tick longer
    // than it takes to hand it to the backend.
    setToken("");
    onSubmit(value, email.trim() || undefined, alias.trim() || undefined)
      .then(() => {
        setEmail("");
        setAlias("");
      })
      .catch(() => {
        // Failure is surfaced via the `error` prop, which the caller owns.
        // Nothing to do here except leave the form open for a retry.
      });
  };

  return (
    <form className="token-form" onSubmit={handleSubmit}>
      <div className="row">
        <label htmlFor="acct-token">Setup token or API key</label>
        <input
          id="acct-token"
          className="input mono"
          type="password"
          autoComplete="off"
          spellCheck={false}
          value={token}
          onChange={(e) => setToken(e.target.value)}
          placeholder="sk-ant-…"
          disabled={pending}
        />
      </div>
      <div className="row">
        <label htmlFor="acct-token-email">Email (optional)</label>
        <input
          id="acct-token-email"
          className="input"
          type="email"
          autoComplete="off"
          value={email}
          onChange={(e) => setEmail(e.target.value)}
          disabled={pending}
        />
      </div>
      <div className="row">
        <label htmlFor="acct-token-alias">Alias (optional)</label>
        <input
          id="acct-token-alias"
          className="input"
          type="text"
          autoComplete="off"
          value={alias}
          onChange={(e) => setAlias(e.target.value)}
          disabled={pending}
        />
      </div>

      {error && (
        <div className="banner danger" role="alert">
          <span>{error}</span>
        </div>
      )}

      <div className="actions">
        <button type="submit" className="btn primary" disabled={pending || token.trim() === ""}>
          {pending ? "Adding…" : "Add token"}
        </button>
        <button type="button" className="btn" onClick={onCancel} disabled={pending}>
          Cancel
        </button>
      </div>
    </form>
  );
}
