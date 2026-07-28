import { quotaState } from "../types";

interface UsageMeterProps {
  /**
   * Utilisation 0..100, or `null`/`undefined` when it is genuinely unknown.
   *
   * Nullable on purpose. This prop used to be a plain `number`, which forced
   * every call site to write `pct ?? 0` — turning "we could not read your
   * usage" into a confident "0%". On a real account sitting at 86% the app
   * displayed 0% in calm grey, which is the single worst thing this product
   * can do: tell you that you have headroom when you are nearly out.
   */
  pct: number | null | undefined;
}

/**
 * The reusable track + fill + percentage.
 *
 * Colour encodes quota state only — at "ok" no colour class is applied, so a
 * healthy meter renders in ink/muted tones like everything else at rest. The
 * percentage is always rendered as text, because state must never be carried
 * by hue alone.
 */
export default function UsageMeter({ pct }: UsageMeterProps) {
  // Unknown is a distinct visual state, never a value. An empty track plus
  // "··" reads as "no reading"; "0%" reads as "no usage".
  if (pct == null || !Number.isFinite(pct)) {
    return (
      <div className="meter" title="Usage could not be read">
        <span className="track" />
        <span className="pct pct-unknown">··</span>
      </div>
    );
  }

  const clamped = Math.max(0, Math.min(100, pct));
  const state = quotaState(clamped);
  const stateClass = state === "ok" ? "" : ` ${state}`;

  return (
    <div className="meter">
      <span className="track">
        <span className={`fill${stateClass}`} style={{ width: `${clamped}%` }} />
      </span>
      <span className={`pct${stateClass}`}>{Math.round(clamped)}%</span>
    </div>
  );
}
