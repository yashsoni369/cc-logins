interface SignInFlowProps {
  /** True while `interactiveLogin()` is in flight — a browser OAuth round trip that can take minutes. */
  pending: boolean;
  /** Message from the most recent failed attempt. `null` on success, on rest, or on a quiet cancellation. */
  error: string | null;
  /** True while some other mutating action holds the shared credential lock. Does not affect the waiting state once started. */
  disabled: boolean;
  /** Invoked only from this component's own button onClick. */
  onStart: () => void;
}

/**
 * "Sign in to another account" — the primary, self-explanatory way to add a
 * login this app does not already have. Opens a real terminal running
 * `claude auth login` against a throwaway config directory and waits for the
 * user to finish a browser OAuth round trip.
 *
 * That wait is the hard part: it can take minutes, so this renders an
 * explanation rather than a bare spinner. Users who don't know a terminal
 * opened elsewhere will otherwise conclude the app has frozen.
 */
export default function SignInFlow({ pending, error, disabled, onStart }: SignInFlowProps) {
  return (
    <div className="signin-flow">
      <button type="button" className="btn primary" disabled={disabled || pending} onClick={onStart}>
        {pending ? "Signing in…" : "Sign in to another account"}
      </button>

      {pending && (
        <div className="signin-wait" role="status">
          <span className="signin-wait-dots" aria-hidden="true">
            <span></span>
            <span></span>
            <span></span>
          </span>
          <div className="signin-wait-copy">
            <p>
              A terminal window has opened, running <code>claude auth login</code>. Switch to it — your browser
              will prompt you to sign in and authorize there.
            </p>
            <p>This can take a minute or two. Nothing here is frozen; it's just waiting on you.</p>
            <p>Closing the terminal at any point cancels safely — nothing is added, and nothing is lost.</p>
          </div>
        </div>
      )}

      {!pending && error && (
        <div className="banner" role="alert">
          <span>{error}</span>
        </div>
      )}
    </div>
  );
}
