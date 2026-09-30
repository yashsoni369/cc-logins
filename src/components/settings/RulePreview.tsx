import { useClockFormat } from "../../lib/clockFormat";
import { predictTarget, projectLimit } from "../../lib/coverage";
import { formatClock } from "../../lib/time";
import { displayName, type Account, type Sample, type Settings } from "../../types";

const STRATEGY_WORDS: Record<Settings["strategy"], string> = {
  "most-headroom": "the account with the most headroom",
  "next-available": "the next available account in your list",
  "consume-first": "the account whose weekly limit resets soonest",
};

function graceWords(seconds: number): string {
  if (seconds <= 0) return "straight away";
  if (seconds < 120) return `after a ${seconds}-second warning`;
  return `after a ${Math.round(seconds / 60)}-minute warning`;
}

/**
 * Auto-switch's three settings read back as one sentence, followed by what
 * that rule would do with the usage on screen right now. A background process
 * that moves credentials has to be legible before it is trusted.
 *
 * The preview is a prediction from local history and says so; the daemon
 * remains the authority on what actually happens.
 */
export default function RulePreview({
  settings,
  threshold,
  accounts,
  activeSamples,
  now,
}: {
  settings: Settings;
  threshold: number;
  accounts: Account[];
  activeSamples: Sample[];
  now: number;
}) {
  const clockFormat = useClockFormat();
  const active = accounts.find((a) => a.active) ?? null;
  const target = predictTarget(accounts, settings.strategy, now);
  const projection = active ? projectLimit(active, activeSamples, now, threshold) : null;

  let preview: string;
  if (!settings.autoSwitchEnabled) {
    preview = "Auto-switch is off, so nothing moves on its own.";
  } else if (!active) {
    preview = "No account is in use yet.";
  } else if (!target) {
    preview = "Right now no other account has measured headroom, so there is nowhere to switch to.";
  } else if (projection?.at == null) {
    preview = `${displayName(active)} isn't burning fast enough to project. When it passes ${threshold}%, it would move to ${displayName(target)}.`;
  } else if (!projection.beforeReset) {
    preview = `At the current pace ${displayName(active)} resets before reaching ${threshold}%, so no switch is expected.`;
  } else if (projection.at <= now) {
    preview = `${displayName(active)} is already past ${threshold}%. It would move to ${displayName(target)} ${graceWords(settings.graceSeconds)}.`;
  } else {
    const when = formatClock(new Date(projection.at).toISOString(), clockFormat, now) ?? "later";
    preview = `With current usage, ${displayName(active)} reaches ${threshold}% around ${when} and would move to ${displayName(target)}.`;
  }

  return (
    <div className="rule">
      <p className="rule-sentence">
        When the account in use passes <b className="num">{threshold}%</b> of any limit, switch to{" "}
        <b>{STRATEGY_WORDS[settings.strategy]}</b> <b>{graceWords(settings.graceSeconds)}</b>.
      </p>
      <p className="rule-preview" role="status">
        <span className="rule-tag">Preview</span> {preview}
      </p>
    </div>
  );
}
