import { useState } from "react";
import {
  checkForUpdate,
  installUpdate,
  type DownloadProgress,
  type UpdateCheck as CheckResult,
} from "../lib/updater";

type Phase =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "result"; result: CheckResult }
  | { kind: "installing"; version: string; progress: DownloadProgress }
  | { kind: "install-failed"; version: string; message: string };

function percent({ downloaded, total }: DownloadProgress): string {
  if (!total) return "…";
  return `${Math.min(100, Math.round((downloaded / total) * 100))}%`;
}

/**
 * The manual update control. Deliberately a button rather than a background
 * check — see the note at the top of `lib/updater.ts` for why this app does not
 * contact anything unless asked.
 */
export default function UpdateCheck() {
  const [phase, setPhase] = useState<Phase>({ kind: "idle" });

  async function onCheck() {
    setPhase({ kind: "checking" });
    setPhase({ kind: "result", result: await checkForUpdate() });
  }

  async function onInstall(result: Extract<CheckResult, { kind: "available" }>) {
    setPhase({
      kind: "installing",
      version: result.version,
      progress: { downloaded: 0, total: null },
    });

    const outcome = await installUpdate(result.update, (progress) =>
      setPhase({ kind: "installing", version: result.version, progress }),
    );

    // On success the app is already restarting, so there is no success state to
    // render — only a failure needs somewhere to go.
    if (!outcome.ok) {
      setPhase({ kind: "install-failed", version: result.version, message: outcome.message });
    }
  }

  const busy = phase.kind === "checking" || phase.kind === "installing";

  return (
    <div className="field">
      <div className="k">
        Updates
        <i>Checked only when you ask.</i>
      </div>
      <div className="v">
        <button className="btn" onClick={() => void onCheck()} disabled={busy}>
          {phase.kind === "checking" ? "Checking…" : "Check for updates"}
        </button>

        {phase.kind === "result" && phase.result.kind === "current" && (
          <span className="about-note">You are on the latest version.</span>
        )}

        {phase.kind === "result" && phase.result.kind === "unsupported" && (
          <span className="about-note">
            Only the desktop app can check for updates — this is the browser preview.
          </span>
        )}

        {phase.kind === "result" && phase.result.kind === "failed" && (
          <span className="about-note">Couldn&apos;t check: {phase.result.message}</span>
        )}

        {phase.kind === "result" && phase.result.kind === "available" && (
          <>
            <span className="about-note">
              Version <span className="num">{phase.result.version}</span> is available. It is
              verified against this build&apos;s signing key before it installs, and the app will
              restart.
            </span>
            {phase.result.notes && <span className="about-note">{phase.result.notes}</span>}
            <button
              className="btn"
              onClick={() => void onInstall(phase.result as Extract<CheckResult, { kind: "available" }>)}
            >
              Download and install {phase.result.version}
            </button>
          </>
        )}

        {phase.kind === "installing" && (
          <span className="about-note">
            Installing {phase.version} — {percent(phase.progress)}. The app will restart on its own.
          </span>
        )}

        {phase.kind === "install-failed" && (
          <span className="about-note">
            Couldn&apos;t install {phase.version}: {phase.message} — download it manually from the
            releases page instead.
          </span>
        )}
      </div>
    </div>
  );
}
