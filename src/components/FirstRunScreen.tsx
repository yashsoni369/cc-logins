import { useState } from "react";

export type FirstRunAction = "signIn" | "addCurrent";

interface FirstRunScreenProps {
  /** Runs an action. Both are user-initiated, never automatic. */
  onAction: (action: FirstRunAction) => void;
  /** The action currently in flight, if any. */
  pending: FirstRunAction | null;
  /** Message from the most recent failed action. */
  error: string | null;
}

/**
 * Zero accounts, and a user who just downloaded a binary that wants to read
 * their authentication tokens. This screen's job is not onboarding — it is
 * earning trust, which is why what the app does and does not do sits right
 * next to the button that hands it a credential.
 *
 * Deliberately shows no example account. An earlier version displayed a
 * hardcoded address as though it had detected one, which is exactly the kind
 * of confident fiction this product is supposed to refuse.
 */
export default function FirstRunScreen({ onAction, pending, error }: FirstRunScreenProps) {
  const [explaining, setExplaining] = useState(false);
  const busy = pending !== null;

  return (
    <div className="pane" style={{ justifyContent: "center" }}>
      <div className="empty">
        <h3>No accounts yet</h3>
        <p>Add a Claude account to start tracking its quota. Everything stays on this machine.</p>

        {error && (
          <div className="banner" role="alert">
            {error}
          </div>
        )}

        <div className="steps">
          <div className="step">
            <span className="i">→</span>
            <span className="t">
              Sign in to an account
              <i>
                Opens a terminal running the official Claude Code sign-in, and your browser prompts
                you there. This app never asks for a password, and whichever account you are
                currently signed into is left untouched.
              </i>
              <button
                className="btn primary"
                type="button"
                disabled={busy}
                onClick={() => onAction("signIn")}
                style={{ marginTop: 8 }}
              >
                {pending === "signIn" ? "Waiting for sign-in…" : "Sign in to an account"}
              </button>
            </span>
          </div>

          <div className="step">
            <span className="i">→</span>
            <span className="t">
              Or add the account you are already signed into
              <i>
                Registers whichever login Claude Code is currently using. It does not open a new
                sign-in.
              </i>
              <button
                className="btn"
                type="button"
                disabled={busy}
                onClick={() => onAction("addCurrent")}
                style={{ marginTop: 8 }}
              >
                {pending === "addCurrent" ? "Adding…" : "Add my current login"}
              </button>
            </span>
          </div>
        </div>

        <button
          className="btn ghost"
          type="button"
          aria-expanded={explaining}
          onClick={() => setExplaining((v) => !v)}
          style={{ marginTop: 4 }}
        >
          {explaining ? "Hide details" : "What does this do?"}
        </button>

        {explaining && (
          <div style={{ fontSize: 12.5, color: "var(--muted)", maxWidth: "56ch", textAlign: "left" }}>
            <p>
              This app keeps a copy of each account&apos;s saved login in its own folder. Switching
              installs the one you pick into Claude Code&apos;s normal location, which is the only
              file it shares with anything else.
            </p>
            <p>
              It reads your quota from Anthropic&apos;s usage endpoint and refreshes tokens so those
              numbers stay current. It never sends a prompt or any model traffic, and there is no
              server — nothing leaves this machine except those usage checks.
            </p>
            <p>
              Automatic switching is off until you turn it on, and when it is on it warns you before
              it moves anything.
            </p>
          </div>
        )}

        <p style={{ fontSize: "11.5px", color: "var(--faint)", maxWidth: "48ch", marginTop: 6 }}>
          Reads usage and hands credentials to the official Claude Code binary. Never routes model
          traffic. No server.
        </p>
      </div>
    </div>
  );
}
