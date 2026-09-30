import { ageLabel, type Environment } from "../../types";

interface EnvironmentsPanelProps {
  environments: Environment[];
  /** Explicit user action only — waking boots a Linux VM. */
  onWake: (envId: string) => void;
  pendingWake: string | null;
  wakeError: { envId: string; message: string } | null;
  mutationInFlight: boolean;
}

function describe(env: Environment): { label: string; on: boolean; detail: string } {
  if (env.status === "asleep") {
    const age = ageLabel(env.lastSeenSeconds);
    return {
      label: "asleep",
      on: false,
      detail: `Stopped, so nothing is read while it sleeps${age ? ` · last reading ${age}` : ""}.`,
    };
  }
  if (env.status === "ignored") return { label: "ignored", on: false, detail: "No Claude Code install here." };
  if (env.hasCredentials === false) return { label: "no login", on: false, detail: "Running, but Claude Code isn't signed in here." };
  if (env.kind === "native") {
    const n = env.accounts.length;
    return { label: "live", on: true, detail: `${n} account${n === 1 ? "" : "s"} managed on this machine.` };
  }
  return {
    label: "live",
    on: true,
    detail: env.hasCredentials ? "Claude Code is signed in here, with its own separate login." : "Running.",
  };
}

/**
 * Where Claude Code keeps logins on this machine. On Windows a WSL distro
 * keeps a login entirely separate from the native one, which is easy to
 * forget, so each is listed plainly. A stopped distro gets an explicit Wake
 * button: reading its files would boot the VM, so that only happens on request.
 */
export default function EnvironmentsPanel({ environments, onWake, pendingWake, wakeError, mutationInFlight }: EnvironmentsPanelProps) {
  if (environments.length <= 1) return null;
  return (
    <section className="band envs" id="environments" aria-label="Environments">
      <div className="band-head">
        <h2>Environments</h2>
        <span className="sub">each keeps its own Claude Code login</span>
      </div>
      <div className="env-list">
        {environments.map((env) => {
          const s = describe(env);
          return (
            <div key={env.id} className="env-row">
              <span className={`env-dot${s.on ? " on" : ""}`} aria-hidden="true" />
              <div className="env-main">
                <div className="env-name">
                  {env.label} <span className={`pill${s.on ? " on" : ""}`}>{s.label}</span>
                </div>
                <div className="env-detail">{s.detail}</div>
                <div className="env-path num" title={env.path}>
                  {env.path}
                </div>
                {wakeError?.envId === env.id && (
                  <div className="row-error" role="alert">
                    {wakeError.message}
                  </div>
                )}
              </div>
              {env.kind === "wsl" && env.status === "asleep" && (
                <button
                  type="button"
                  className="btn"
                  disabled={mutationInFlight || pendingWake !== null}
                  title="Starts the distro so its login can be checked."
                  onClick={() => onWake(env.id)}
                >
                  {pendingWake === env.id ? "Waking…" : "Wake & check"}
                </button>
              )}
            </div>
          );
        })}
      </div>
    </section>
  );
}
