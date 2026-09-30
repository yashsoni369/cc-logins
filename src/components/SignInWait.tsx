/**
 * What to say while `claude auth login` runs in its own terminal. Exported so
 * every place that can start a sign-in uses the same words.
 */
export function SignInWait() {
  return (
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
  );
}
