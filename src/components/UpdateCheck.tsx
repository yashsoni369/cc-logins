import type { UseUpdateResult } from "../lib/useUpdate";
import type { DownloadProgress } from "../lib/updater";

function percent({ downloaded, total }: DownloadProgress): string {
  if (!total) return "…";
  return `${Math.min(100, Math.round((downloaded / total) * 100))}%`;
}

/**
 * The update row in About. Holds no state of its own — `useUpdate` is mounted
 * once in `App`, so what is shown here is the same check the background
 * scheduler ran, not a second opinion.
 */
export default function UpdateCheck({ update }: { update: UseUpdateResult }) {
  const { status, checking, install, blocked } = update;
  const busy = checking || install.kind === "installing";

  return (
    <div className="field">
      <div className="k">
        Updates
        <i>Verified against this build&apos;s signing key before installing.</i>
      </div>
      <div className="v">
        <button className="btn" onClick={() => void update.check()} disabled={busy}>
          {checking ? "Checking…" : "Check for updates"}
        </button>

        {status?.kind === "current" && (
          <span className="about-note">You are on the latest version.</span>
        )}

        {status?.kind === "unsupported" && (
          <span className="about-note">
            Only the desktop app can check for updates — this is the browser preview.
          </span>
        )}

        {status?.kind === "failed" && (
          <span className="about-note">Couldn&apos;t check: {status.message}</span>
        )}

        {status?.kind === "available" && install.kind !== "installing" && (
          <>
            <span className="about-note">
              Version <span className="num">{status.version}</span> is available. The app will
              restart once it installs.
            </span>
            {status.notes && <span className="about-note">{status.notes}</span>}

            {/* Restarting mid-switch would interrupt a credential rotation. */}
            {blocked ? (
              <span className="about-note">{blocked}</span>
            ) : (
              <button className="btn" onClick={() => void update.startInstall()}>
                Download and install {status.version}
              </button>
            )}
          </>
        )}

        {install.kind === "installing" && (
          <span className="about-note">
            Installing — {percent(install.progress)}. The app will restart on its own.
          </span>
        )}

        {install.kind === "failed" && (
          <span className="about-note">
            Couldn&apos;t install: {install.message} — download it manually from the releases page
            instead.
          </span>
        )}
      </div>
    </div>
  );
}
