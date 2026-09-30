import { useState } from "react";

export type FirstRunAction = "signIn" | "addCurrent";

interface FirstRunScreenProps {
  /** Runs an action. Both are user-initiated, never automatic. */
  onAction: (action: FirstRunAction) => void;
  /** The action currently in flight, if any. */
  pending: FirstRunAction | null;
  /** Message from the most recent failed action. */
  error: string | null;
  /**
   * Whether Claude Code appears to hold a login on this machine.
   * `undefined` means it could not be determined without a read that might
   * prompt — treat that as "possibly", never as "no".
   */
  loginPresent?: boolean;
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
export default function FirstRunScreen({
  onAction,
  pending,
  error,
  loginPresent,
}: FirstRunScreenProps) {
  const [explaining, setExplaining] = useState(false);
  const busy = pending !== null;
  // Only `false` is a confident "there is no login here"; undefined is the
  // undetermined case and gets the same encouraging path as a found one.
  const maybeSignedIn = loginPresent !== false;

  return (
    <div className="pane" style={{ justifyContent: "center" }}>
      <div className="empty">
        <h3>{loginPresent === true ? "Found your Claude Code login" : "Not tracking any accounts yet"}</h3>
        <p>
          {loginPresent === true
            ? "Use it below to start tracking its quota, or sign in to a different account. This app never stores or refreshes your Claude login."
            : "Add a Claude account to start tracking its quota. This app never stores or refreshes your Claude login."}
        </p>

        {error && (
          <div className="banner" role="alert">
            {error}
          </div>
        )}

        {/*
          Ordered by what the machine can see. Someone already signed into
          Claude Code wants the one-click path, and leading with "sign in"
          reads as though the app had not noticed — which is the complaint
          this screen earned: it told a subscriber they had no account.
        */}
        <div className="steps">
          {maybeSignedIn && (
            <div className="step">
              <span className="i">→</span>
              <span className="t">
                {loginPresent === true
                  ? "Use the account Claude Code is signed into"
                  : "Use the account Claude Code is already signed into"}
                <i>
                  It keeps using Claude Code&apos;s own folder. Nothing is copied, and no new
                  sign-in opens.
                </i>
                <button
                  className="btn primary"
                  type="button"
                  disabled={busy}
                  onClick={() => onAction("addCurrent")}
                  style={{ marginTop: 8 }}
                >
                  {pending === "addCurrent" ? "Adding…" : "Use my current login"}
                </button>
              </span>
            </div>
          )}

          <div className="step">
            <span className="i">→</span>
            <span className="t">
              {maybeSignedIn ? "Or sign in to another account" : "Sign in to an account"}
              <i>
                Opens a terminal running the official <code>claude auth login</code>, into a new
                folder for that account, and your browser prompts you there. This app never asks
                for a password, and your current login is left untouched.
              </i>
              <button
                className={maybeSignedIn ? "btn" : "btn primary"}
                type="button"
                disabled={busy}
                onClick={() => onAction("signIn")}
                style={{ marginTop: 8 }}
              >
                {pending === "signIn" ? "Waiting for sign-in…" : "Sign in to an account"}
              </button>
            </span>
          </div>

          {!maybeSignedIn && (
          <div className="step">
            <span className="i">→</span>
            <span className="t">
              Or use the account Claude Code is already signed into
              <i>
                It keeps using Claude Code&apos;s own folder. Nothing is copied, and no new
                sign-in opens.
              </i>
              <button
                className="btn"
                type="button"
                disabled={busy}
                onClick={() => onAction("addCurrent")}
                style={{ marginTop: 8 }}
              >
                {pending === "addCurrent" ? "Adding…" : "Use my current login"}
              </button>
            </span>
          </div>
          )}
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
          <div style={{ fontSize: 12.5, color: "var(--muted)", textAlign: "left" }}>
            <p>
              Each account signs in once, through Claude&apos;s own sign-in, into its own Claude
              Code folder. The login stays there. This app never stores, copies or refreshes it.
            </p>
            <p>
              Picking an account changes which folder new Claude Code sessions start with.
              Sessions already running keep their account.
            </p>
            <p>
              To show quota, it reads an account&apos;s current token where Claude Code keeps it,
              for one request to Anthropic&apos;s usage endpoint. It never sends a prompt or any
              model traffic, and there is no server.
            </p>
            <p>
              Automatic switching is off until you turn it on, and when it is on it warns you before
              it picks another account.
            </p>
          </div>
        )}

        <p style={{ fontSize: "11.5px", color: "var(--faint)", marginTop: 6 }}>
          Reads usage only. Never stores your login, never routes model traffic. No server.
        </p>
      </div>
    </div>
  );
}
