import type { Account, QuotaState } from "../../types";
import { bindingUtilisation, displayName, formatSpend, isEnterprise, quotaState } from "../../types";
import { formatCountdown } from "../../lib/time";
import { useElementWidth } from "../../lib/useElementWidth";

interface ResetStaggerProps {
  accounts: Account[];
  now: number;
  /** Horizon in hours. 12 covers more than two whole 5-hour windows. */
  hours?: number;
}

/**
 * User units are kept at roughly one-to-one with rendered pixels. The strip is
 * full-pane width, so a narrow viewBox would scale every `font-size` up with
 * it — at 320 units wide the labels rendered three times over and collided
 * with the bands. Band heights and type sizes below are therefore real pixels.
 */
/**
 * Fallback until the container is measured. The width is no longer fixed:
 * the pane is allowed to grow past its old cap, and a fixed viewBox scaled to
 * fit would have enlarged every label along with it — the very failure this
 * file's header records.
 */
const VIEW_W_FALLBACK = 1000;
// Sized to the widest name `clip` will emit, not to a fraction of the width:
// at 200 the gutter held 12 characters of 11px type in 200px of room.
const TRACK_X = 122;
// A 10px label cannot sit inside a 9px band; the text crossed the outline.
const ROW_PITCH = 18;
const BAND_H = 12;
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
  return name.length > 17 ? `${name.slice(0, 16)}…` : name;
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
  const [wrapRef, VIEW_W] = useElementWidth<HTMLDivElement>(VIEW_W_FALLBACK);
  const TRACK_W = Math.max(120, VIEW_W - TRACK_X - 4);

  if (accounts.length === 0) {
    return <div className="stagger dash-cap">No accounts to plot.</div>;
  }

  const horizonMs = hours * 3_600_000;
  /*
   * Enterprise accounts are plotted separately, below.
   *
   * This strip exists to show the gaps between hourly resets — the stretch
   * where every account is still on spent quota is what strands you. A monthly
   * spend cap is always off the right-hand edge of a 12-hour horizon, so a band
   * for it would be a full-width bar that never moves, saying nothing while
   * taking a row from the accounts the strip is about.
   */
  const hourly = accounts.filter((a) => !isEnterprise(a.usage));
  const monthly = accounts.filter((a) => isEnterprise(a.usage));
  const bands = hourly.map((a) => bandFor(a, now, horizonMs));
  const rowsH = bands.length * ROW_PITCH;
  const viewH = rowsH + AXIS_H;
  const title = describe(bands, hours);
  const axisY = viewH - 2;

  return (
    <div className="stagger" ref={wrapRef}>
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
              <text className="stagger-axis" x={0} y={y + BAND_H - 1.5} fontSize={11}>
                {clip(b.name)}
              </text>

              {/* Outline first, always: it carries the band's full extent at a
                  neutral 6:1, so the replenished stretch stays legible without
                  depending on a tint of the state colour. */}
              <rect className="stagger-track" x={TRACK_X} y={y} width={TRACK_W} height={BAND_H} />

              {b.at == null ? (
                /* An account whose reset we cannot read is a real hole in the
                   plan, so it keeps its outline and says so. */
                <text
                  className="stagger-axis"
                  x={TRACK_X + TRACK_W / 2}
                  y={y + BAND_H - 3.5}
                  fontSize={10}
                  textAnchor="middle"
                >
                  reset unknown
                </text>
              ) : (
                <>
                  {/* Consumed is filled, replenished is the bare track — the
                      same filled-versus-empty vocabulary as the quota meters,
                      and it survives at any contrast. */}
                  {split > 0 && (
                    <rect className={cls} x={TRACK_X} y={y} width={split} height={BAND_H} fill={fill} />
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

        <text className="stagger-axis" x={TRACK_X} y={axisY} fontSize={10}>
          now
        </text>
        <text className="stagger-axis" x={TRACK_X + TRACK_W / 2} y={axisY} fontSize={10} textAnchor="middle">
          +{Math.round(hours / 2)}h
        </text>
        <text className="stagger-axis" x={TRACK_X + TRACK_W} y={axisY} fontSize={10} textAnchor="end">
          +{hours}h
        </text>
      </svg>

      {/*
        Accounts on a monthly cap, on their own line and their own scale.
        Rendered only when there are any — a permanent empty row explaining an
        absent account type is worse than the omission it explains.
      */}
      {monthly.length > 0 && (
        <ul className="stagger-monthly">
          {monthly.map((account) => {
            const spend = account.usage?.spend;
            const countdown = formatCountdown(spend?.resetsAt, now);
            return (
              <li key={account.number}>
                <span className="nm" title={displayName(account)}>
                  {displayName(account)}
                </span>
                <span className="num">
                  {spend ? formatSpend(spend) : "spend cap"} · monthly cap
                  {countdown ? `, resets in ${countdown}` : ""}
                </span>
              </li>
            );
          })}
        </ul>
      )}
    </div>
  );
}
