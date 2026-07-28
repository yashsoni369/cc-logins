import { ageLabel } from "../types";
import UsageMeter from "./UsageMeter";

/**
 * A meter track with no known value yet — distinct from `UsageMeter` at 0%,
 * which would claim the quota is empty. Loading is "unknown", not "zero".
 */
function PendingMeter() {
  return (
    <div className="meter">
      <span className="track">
        <span className="fill" style={{ width: "100%", background: "var(--raised)" }} />
      </span>
      <span className="pct" style={{ color: "var(--faint)" }}>
        ··
      </span>
    </div>
  );
}

/**
 * Wraps an already-known meter at reduced opacity, per the hard rule: stale
 * data renders dimmed, never blank, never silently fresh-looking. Reuses
 * UsageMeter rather than re-implementing meter markup.
 */
function StaleMeter({ pct }: { pct: number }) {
  return (
    <div style={{ opacity: 0.55 }}>
      <UsageMeter pct={pct} />
    </div>
  );
}

/**
 * First paint: account names are known instantly from the local store, but
 * usage hasn't been read yet. No spinner over the panel — only the meters
 * are pending, so what's already known keeps rendering.
 */
export function LoadingState() {
  return (
    <div className="realm">
      <div className="realm-head">
        <span className="who">
          <span className="mark on"></span>
          <span className="alias">naim</span>
        </span>
        <span className="sp"></span>
        <span className="pill">reading</span>
      </div>
      <div className="realm-body" style={{ display: "flex", flexDirection: "column", gap: 7, padding: "10px 14px" }}>
        <PendingMeter />
        <PendingMeter />
      </div>
      <div className="realm-foot">Account names load instantly; usage follows.</div>
    </div>
  );
}

/** Can't reach the usage API. Names what still works, not just what failed. */
export function NetworkUnreachableState() {
  return (
    <div className="realm">
      <div className="banner">
        <span style={{ color: "var(--muted)" }}>Can&apos;t reach Anthropic — retrying in 40s</span>
      </div>
      <div className="realm-head">
        <span className="who">
          <span className="mark on"></span>
          <span className="alias">naim</span>
        </span>
        <span className="sp"></span>
        <span className="pill">{ageLabel(360) ?? "6m old"}</span>
      </div>
      <div className="realm-body" style={{ display: "flex", flexDirection: "column", gap: 7, padding: "10px 14px" }}>
        <StaleMeter pct={61} />
        <StaleMeter pct={13} />
      </div>
      <div className="realm-foot">Switching still works offline.</div>
    </div>
  );
}

/** A saved login was rejected. The account is held out of rotation until fixed. */
export function LoginExpiredState() {
  return (
    <div className="realm">
      <div className="banner danger">
        <span>work needs signing in again</span>
      </div>
      <div className="realm-head">
        <span className="who">
          <span className="mark"></span>
          <span className="alias">work</span>
        </span>
        <span className="sp"></span>
        <span className="pill danger">expired</span>
      </div>
      <div className="realm-body">
        <p style={{ margin: "10px 0", fontSize: 12, color: "var(--muted)" }}>
          Its saved login was rejected. It is held out of rotation until you sign in again.
        </p>
      </div>
      <div className="realm-foot">
        <button className="btn">Sign in to work</button>
      </div>
    </div>
  );
}

/** The usage API's own rate limit slowed polling. Explained, not hidden. */
export function RateLimitedState() {
  return (
    <div className="realm">
      <div className="banner">
        <span style={{ color: "var(--muted)" }}>Checking less often for a while</span>
      </div>
      <div className="realm-head">
        <span className="who">
          <span className="mark on"></span>
          <span className="alias">naim</span>
        </span>
        <span className="sp"></span>
        <span className="pill">{ageLabel(120) ?? "2m old"}</span>
      </div>
      <div className="realm-body" style={{ padding: "10px 14px" }}>
        <StaleMeter pct={61} />
        <p style={{ margin: "10px 0 0", fontSize: 12, color: "var(--muted)" }}>
          Anthropic limits how often usage can be read. Polling has slowed to stay inside it.
        </p>
      </div>
    </div>
  );
}

/**
 * Demo gallery of the four states above, for visual review — each state is
 * also exported individually for use wherever the app needs to render it.
 */
export default function DegradedStatesGallery() {
  return (
    <div className="pane">
      <div className="pane-head">
        <h3>Degraded states</h3>
      </div>
      <div style={{ display: "flex", flexDirection: "column", gap: 22, maxWidth: 360 }}>
        <LoadingState />
        <NetworkUnreachableState />
        <LoginExpiredState />
        <RateLimitedState />
      </div>
    </div>
  );
}
