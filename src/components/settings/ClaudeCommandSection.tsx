import { useCallback, useEffect, useRef, useState } from "react";
import { cliStatus, installCli, IpcError, uninstallCli } from "../../lib/api";
import type { CliStatus } from "../../types";
import { useToast } from "../ui/ToastRegion";

/** `undefined` while loading, `null` when there is no backend. */
type Loadable<T> = T | null | undefined;

function errorText(error: unknown): string {
  if (error instanceof IpcError) return error.detail ?? error.message;
  return String(error);
}

/** The editor setting that routes VS Code's Claude panel through the shim. */
export function vscodeSetting(commandPath: string): string {
  return `"claudeCode.claudeProcessWrapper": ${JSON.stringify(commandPath)}`;
}

/**
 * Settings › Claude command. Installs the `claude` shim first on PATH so new
 * terminals start the account picked for new sessions. Explicit and
 * reversible; the app never edits editor settings, it shows the line to add.
 */
export default function ClaudeCommandSection() {
  const toast = useToast();
  const [status, setStatus] = useState<Loadable<CliStatus>>(undefined);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const mounted = useRef(true);

  useEffect(() => {
    mounted.current = true;
    cliStatus()
      .then((result) => {
        if (mounted.current) setStatus(result.data);
      })
      .catch((e: unknown) => {
        if (!mounted.current) return;
        setStatus(null);
        setError(errorText(e));
      });
    return () => {
      mounted.current = false;
    };
  }, []);

  const run = useCallback(
    async (action: () => Promise<CliStatus>, done: string) => {
      setBusy(true);
      setError(null);
      try {
        const next = await action();
        if (!mounted.current) return;
        setStatus(next);
        toast.show({ message: done });
      } catch (e) {
        if (mounted.current) setError(errorText(e));
      } finally {
        if (mounted.current) setBusy(false);
      }
    },
    [toast],
  );

  const copy = useCallback(
    (text: string) => {
      navigator.clipboard
        .writeText(text)
        .then(() => toast.show({ message: "Copied", duration: 2_000 }))
        .catch(() => toast.show({ message: "Couldn't copy. Select the text instead." }));
    },
    [toast],
  );

  const health = status?.health.state;
  const installed = health === "installed" || health === "shadowed";

  return (
    <section className="settings-group" aria-labelledby="set-cli">
      <h2 id="set-cli">Claude command</h2>
      <div className="fields">
        <div className="field">
          <div className="k">
            The <code>claude</code> command
            <i>
              Puts CC Logins&apos; <code>claude</code> first on your PATH, so new terminals start the account you
              pick here. Sessions already running keep their account.
            </i>
          </div>
          <div className="v">
            <span role="status" className="cli-status">
              {status === undefined && <span className="muted">Checking…</span>}
              {status === null && !error && <span className="muted">Available in the desktop app.</span>}
              {status && !status.shimAvailable && !installed && (
                <span className="muted">This build doesn&apos;t include it. Install CC Logins from a release.</span>
              )}
              {status && status.shimAvailable && health === "notInstalled" && (
                <span className="muted">Not installed. Plain claude runs your default account.</span>
              )}
              {health === "installed" && <span className="muted">Installed. New terminals use it.</span>}
              {status?.health.state === "shadowed" && (
                <span className="cli-warn">
                  Installed, but {status.health.by} comes first on PATH. Move {status.binDir} ahead of it, or
                  remove the other entry.
                </span>
              )}
            </span>
            {status && status.shimAvailable && health === "notInstalled" && (
              <button
                type="button"
                className="btn primary"
                disabled={busy}
                onClick={() => void run(installCli, "Installed. Open a new terminal to use it.")}
              >
                {busy ? "Installing…" : "Install"}
              </button>
            )}
            {installed && (
              <button
                type="button"
                className="btn ghost"
                disabled={busy}
                onClick={() => void run(uninstallCli, "Removed. New terminals use Claude Code directly.")}
              >
                {busy ? "Removing…" : "Uninstall"}
              </button>
            )}
            {error && <span className="field-error">{error}</span>}
          </div>
        </div>

        {status && installed && status.launchers.length > 0 && (
          <div className="field">
            <div className="k">
              Per-account commands
              <i>Always start that account, whatever is picked for new sessions. Handy side by side.</i>
            </div>
            <div className="v cli-launchers">
              {status.launchers.map((launcher) => (
                <div key={launcher.slug} className="cli-row">
                  <code>{launcher.command}</code>
                  <button type="button" className="btn ghost btn-sm" onClick={() => copy(launcher.command)}>
                    Copy
                  </button>
                </div>
              ))}
            </div>
          </div>
        )}

        {status && installed && (
          <div className="field">
            <div className="k">
              VS Code
              <i>
                The Claude Code panel starts its own claude. Add this line to your VS Code settings so it uses
                this one too.
              </i>
            </div>
            <div className="v cli-row">
              <code className="cli-snippet">{vscodeSetting(status.commandPath)}</code>
              <button
                type="button"
                className="btn ghost btn-sm"
                onClick={() => copy(vscodeSetting(status.commandPath))}
              >
                Copy
              </button>
            </div>
          </div>
        )}
      </div>
    </section>
  );
}
