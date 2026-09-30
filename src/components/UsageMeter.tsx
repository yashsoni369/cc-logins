import { useDisplayMode } from "../lib/displayMode";
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
  /**
   * Where utilisation *should* be by now in this window (0..100), drawn as a
   * tick. Fill past the tick means spending faster than the window lasts.
   */
  pace?: number | null;
}

/**
 * The reusable track + fill + percentage.
 *
 * Colour encodes quota state only — at "ok" no colour class is applied, so a
 * healthy meter renders in ink/muted tones like everything else at rest. The
 * percentage is always rendered as text, because state must never be carried
 * by hue alone.
 *
 * In "left" display mode the text reads what remains, but fill and colour
 * still follow utilisation: a nearly-full account stays red and nearly full
 * whichever way the number is phrased.
 */
export default function UsageMeter({ pct, pace }: UsageMeterProps) {
  const mode = useDisplayMode();
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
  const shown = Math.round(mode === "left" ? 100 - clamped : clamped);
  const paceAt = pace != null && Number.isFinite(pace) ? Math.max(0, Math.min(100, pace)) : null;

  return (
    <div className="meter">
      <span className="track">
        {/* Scaled rather than resized: a transform animates on the compositor,
            so a refresh that moves every meter at once costs no layout. */}
        <span className={`fill${stateClass}`} style={{ transform: `scaleX(${clamped / 100})` }} />
        {paceAt !== null && (
          <span
            className={`pace${clamped > paceAt + 1 ? " ahead" : ""}`}
            style={{ left: `${paceAt}%` }}
            title={`On pace would be ${Math.round(paceAt)}% by now`}
          />
        )}
      </span>
      <span className={`pct${stateClass}`} key={shown}>
        {shown}%{mode === "left" && <span className="pct-unit"> left</span>}
      </span>
    </div>
  );
}
