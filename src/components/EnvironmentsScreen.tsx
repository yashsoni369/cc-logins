import type { Environment } from "../types";
import { ageLabel, displayName } from "../types";
import UsageMeter from "./UsageMeter";

interface EnvironmentsScreenProps {
  environments: Environment[];
}

function RealmStatusPill({ status, hasCredentials }: { status: Environment["status"]; hasCredentials?: boolean }) {
  if (status === "live") {
    // A running realm with no Claude Code install reads the same neutral
    // way "asleep"/"ignored" do below — `on` (ink-highlighted) is reserved
    // for a realm that's actually live *and* has something to show.
    if (hasCredentials === false) return <span className="pill">no install</span>;
    return <span className="pill on">live</span>;
  }
  if (status === "asleep") return <span className="pill">asleep</span>;
  return <span className="pill">ignored</span>;
}

/** A readable realm: an accounts table, same primitives as the Accounts screen. */
function LiveBody({ env }: { env: Environment }) {
  return (
    <div className="realm-body">
      <table className="accts">
        <tbody>
          {env.accounts.map((account) => (
            <tr key={account.number}>
              <td style={{ width: "36%" }}>
                <div className="who">
                  <span className={`mark${account.active ? " on" : ""}`}></span>
                  <span className="alias">{displayName(account)}</span>
                  {account.active && <span className="pill on">active</span>}
                </div>
              </td>
              <td>
                <UsageMeter pct={account.usage?.fiveHour?.pct} />
              </td>
              <td className="r">
                <button className="btn ghost">Switch</button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/**
 * A running realm that was probed for Claude Code credentials and found
 * none (`hasCredentials === false`, not just an empty/not-yet-populated
 * `accounts` list). Reuses the same "prose explanation, no data table"
 * treatment as `AsleepBody` below — both are "nothing to show, here's why"
 * states, just for different reasons — so an empty `LiveBody` table is never
 * shown for a realm that's confirmed to have no install.
 */
function NoInstallBody() {
  return (
    <div className="realm-body no-install">
      <p>This realm is running, but no Claude Code install was found here.</p>
    </div>
  );
}

/**
 * A stopped WSL distro. Reading its credentials would boot the VM, so this
 * realm is never polled while asleep — the body shows the last-known reading
 * with its age, in prose, plus an explicit wake action. Never blank, never
 * silently fresh-looking.
 */
function AsleepBody({ env }: { env: Environment }) {
  const age = ageLabel(env.lastSeenSeconds);
  const known = env.accounts[0];

  return (
    <div className="realm-body asleep">
      <p>
        This distro is stopped. Reading its credentials would start it, so nothing is polled while it sleeps.
        {known ? (
          <>
            {" "}
            Last known reading is <span className="num">{age ?? "just now"}</span>: <b>{displayName(known)}</b>{" "}
            active at{" "}
            <span className="num">
              {known.usage?.fiveHour?.pct == null
                ? "an unrecorded level"
                : `${Math.round(known.usage.fiveHour.pct)}%`}
            </span>
            .
          </>
        ) : (
          " No reading has ever been taken."
        )}
      </p>
      <button className="btn" style={{ marginLeft: "auto", flex: "none" }}>
        Wake &amp; refresh
      </button>
    </div>
  );
}

export default function EnvironmentsScreen({ environments }: EnvironmentsScreenProps) {
  return (
    <div className="pane">
      <div className="pane-head">
        <h3>Environments</h3>
        <span className="sub">{environments.length} found</span>
      </div>

      {environments.map((env) => (
        <div className="realm" key={env.id}>
          <div className="realm-head">
            <span className="t">{env.label}</span>
            <span className="p num">{env.path}</span>
            <span className="sp"></span>
            <RealmStatusPill status={env.status} hasCredentials={env.hasCredentials} />
          </div>
          {env.status === "live" && env.hasCredentials === false && <NoInstallBody />}
          {env.status === "live" && env.hasCredentials !== false && <LiveBody env={env} />}
          {env.status === "asleep" && <AsleepBody env={env} />}
        </div>
      ))}
    </div>
  );
}
