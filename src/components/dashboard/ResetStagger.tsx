import type { Account, QuotaState } from "../../types";
import { bindingUtilisation, displayName, quotaState } from "../../types";
import { formatCountdown } from "../../lib/time";

interface ResetStaggerProps {
  accounts: Account[];
  now: number;
  /** Horizon in hours. 12 covers more than two whole 5-hour windows. */
  hours?: number;
}

const VIEW_W = 320;
const TRACK_X = 66;
const TRACK_W = VIEW_W - TRACK_X - 4;
const ROW_PITCH = 15;
const BAND_H = 9;
const AXIS_H = 12;

/**
 * Fills come from the -fill tokens, which are tuned for bars rather than text.
 * They are set as presentation attributes, not inline styles: attributes sit at
 * the bottom of the cascade, so `.stagger-band` rules still win, but a band can
 * never render colourless if the stylesheet has not caught up yet.
 */
const FILL: Record<QuotaState, string> = {
  ok: "var(--ok-fill)",
  caution: "var(--caution-fill)",
  danger: "var(--danger-fill)",
};

interface Band {
  key: number;
  name: string;
  state: QuotaState;
  /** Reset position as a fraction of the horizon. `null` = unknown, `>1` = beyond it. */
  at: number | null;
  countdown: string | null;
}

function bandFor(account: Account, now: number, horizonMs: number): Band {
  const resetsAt = account.usage?.fiveHour?.resetsAt;
  // Parsed here rather than leaning on formatCountdown alone: the band needs the
  // position and the words to come from the same instant, or they disagree.
  const ms = resetsAt ? Date.parse(resetsAt) : Number.NaN;
  return {
    key: account.number,
    name: displayName(account),
    state: quotaState(bindingUtilisation(account.usage)),
    at: Number.isFinite(ms) ? (ms - now) / horizonMs : null,
    countdown: formatCountdown(resetsAt, now),
  };
}

/** Labels are drawn, not laid out, so they cannot ellipsize themselves. */
function clip(name: string): string {
  return name.length > 12 ? `${name.slice(0, 11)}…` : name;
}

function listNames(bands: Band[]): string {
  const names = bands.map((b) => b.name);
  if (names.length <= 1) return names.join("");
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1] ?? ""}`;
}

function bandTitle(b: Band): string {
  if (b.at == null) return `${b.name}: no known reset time`;
  if (b.at > 1) return `${b.name}: no reset within the window`;
  return `${b.name}: resets in ${b.countdown ?? "under a minute"}`;
}

/**
 * The stagger carries its meaning in position and colour, so the conclusion has
 * to survive as prose too — a coverage gap is the one thing a screen reader user
 * must never be left to infer from a shape they cannot see.
 */
function describe(bands: Band[], hours: number): string {
  const within = bands.filter((b) => b.at != null && b.at <= 1).sort((a, b) => (a.at ?? 0) - (b.at ?? 0));
  const beyond = bands.filter((b) => b.at != null && b.at > 1);
  const unknown = bands.filter((b) => b.at == null);
  const rested = bands.filter((b) => b.state === "ok");
  const soonest = within[0];

  const parts = [
    `Reset stagger for ${bands.length} account${bands.length === 1 ? "" : "s"} over the next ${hours} hours.`,
  ];

  if (rested.length > 0) {
    parts.push(
      `${rested.length} of ${bands.length} still ${rested.length === 1 ? "has" : "have"} headroom now, so there is no coverage gap.`,
    );
  } else if (soonest) {
    parts.push(
      `Every account is at or near its limit; the first relief is ${soonest.name} in ${soonest.countdown ?? "under a minute"}, so there is no headroom until then.`,
    );
  } else {
    parts.push(
      `Every account is at or near its limit and none resets within the next ${hours} hours — there is no headroom in this window.`,
    );
  }

  if (within.length) {
    parts.push(
      `Resets due: ${within.map((b) => `${b.name} in ${b.countdown ?? "under a minute"}`).join(", ")}.`,
    );
  }
  if (beyond.length) {
    parts.push(`${listNames(beyond)} ${beyond.length === 1 ? "does" : "do"} not reset within the window.`);
  }
  if (unknown.length) {
    parts.push(`${listNames(unknown)} ${unknown.length === 1 ? "has" : "have"} no known reset time.`);
  }
  return parts.join(" ");
}

/**
 * Where every account's 5-hour window replenishes, on one shared clock.
 *
 * The point is not the individual resets, it is the gaps between them: a
 * vertical slice where every band is still on consumed quota is exactly the
 * stretch that strands you, and it is invisible on any per-account view.
 */
export default function ResetStagger({ accounts, now, hours = 12 }: ResetStaggerProps) {
  if (accounts.length === 0) {
    return <div className="stagger dash-cap">No accounts to plot.</div>;
  }

  const horizonMs = hours * 3_600_000;
  const bands = accounts.map((a) => bandFor(a, now, horizonMs));
  const rowsH = bands.length * ROW_PITCH;
  const viewH = rowsH + AXIS_H;
  const title = describe(bands, hours);
  const axisY = viewH - 2;

  return (
    <div className="stagger">
      <svg viewBox={`0 0 ${VIEW_W} ${viewH}`} preserveAspectRatio="xMidYMid meet" role="img" aria-label={title}>
        <title>{title}</title>

        {[0, 0.5, 1].map((f) => (
          <line
            key={f}
            className="stagger-axis"
            x1={TRACK_X + f * TRACK_W}
            x2={TRACK_X + f * TRACK_W}
            y1={0}
            y2={rowsH}
            stroke="var(--line-soft)"
          />
        ))}

        {bands.map((b, i) => {
          const y = i * ROW_PITCH + (ROW_PITCH - BAND_H) / 2;
          const cls = `stagger-band${b.state === "ok" ? "" : ` ${b.state}`}`;
          const fill = FILL[b.state];
          // Clamped, not dropped: a reset beyond the horizon fills the whole
          // band with consumed quota, which is the strand signal itself.
          const split = b.at == null ? 0 : Math.max(0, Math.min(1, b.at)) * TRACK_W;

          return (
            <g key={b.key}>
              <title>{bandTitle(b)}</title>
              <text className="stagger-axis" x={0} y={y + BAND_H - 1.5} fontSize={7}>
                {clip(b.name)}
              </text>

              {b.at == null ? (
                <>
                  {/* Never omitted and never drawn at zero — an account whose
                      reset we cannot read is a real hole in the plan. */}
                  <rect
                    className={`${cls} unknown`}
                    x={TRACK_X}
                    y={y}
                    width={TRACK_W}
                    height={BAND_H}
                    fill="var(--faint)"
                    opacity={0.22}
                  />
                  <text
                    className="stagger-axis"
                    x={TRACK_X + TRACK_W / 2}
                    y={y + BAND_H - 1.5}
                    fontSize={6.5}
                    textAnchor="middle"
                  >
                    reset unknown
                  </text>
                </>
              ) : (
                <>
                  {/* Consumed then replenished, separated by opacity rather than
                      a second hue: one band, one state, one colour. */}
                  {split > 0 && (
                    <rect className={cls} x={TRACK_X} y={y} width={split} height={BAND_H} fill={fill} opacity={0.9} />
                  )}
                  {split < TRACK_W && (
                    <rect
                      className={cls}
                      x={TRACK_X + split}
                      y={y}
                      width={TRACK_W - split}
                      height={BAND_H}
                      fill={fill}
                      opacity={0.22}
                    />
                  )}
                  {split > 0 && split < TRACK_W && (
                    <rect
                      className={cls}
                      x={TRACK_X + split - 0.75}
                      y={y - 1.5}
                      width={1.5}
                      height={BAND_H + 3}
                      fill={fill}
                    />
                  )}
                </>
              )}
            </g>
          );
        })}

        <text className="stagger-axis" x={TRACK_X} y={axisY} fontSize={6.5}>
          now
        </text>
        <text className="stagger-axis" x={TRACK_X + TRACK_W / 2} y={axisY} fontSize={6.5} textAnchor="middle">
          +{Math.round(hours / 2)}h
        </text>
        <text className="stagger-axis" x={TRACK_X + TRACK_W} y={axisY} fontSize={6.5} textAnchor="end">
          +{hours}h
        </text>
      </svg>
    </div>
  );
}
