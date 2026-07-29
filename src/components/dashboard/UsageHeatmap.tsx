import { useId } from "react";

import type { Sample } from "../../types";

interface UsageHeatmapProps {
  /** Intraday samples, ascending. May be empty. */
  samples: Sample[];
}

/** Monday-first: a working week reads better here than a calendar one. */
const WEEKDAYS = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const WEEKDAY_LONG = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

/**
 * Six-hour parts of day rather than 24 hourly rows. On a real machine a poll
 * every few minutes still leaves most hourly cells empty, and a grid that is
 * mostly "no data" answers nothing; four rows keep enough samples per bucket
 * for its average to mean something.
 */
const PARTS = [
  { label: "Night", hours: "00:00–06:00" },
  { label: "Morning", hours: "06:00–12:00" },
  { label: "Afternoon", hours: "12:00–18:00" },
  { label: "Evening", hours: "18:00–24:00" },
];

const CELLS = WEEKDAYS.length * PARTS.length;

/**
 * Faintest a sampled cell may be drawn. A bucket that genuinely averages 0% is
 * a measurement, not an absence — at opacity 0 it would be indistinguishable
 * from a bucket nobody has sampled, which is the one mistake this grid exists
 * to avoid. Absence gets its own class instead, never a lower intensity.
 */
const FLOOR = 0.16;

interface Bucket {
  sum: number;
  count: number;
}

/** `Date.getDay()` is Sunday-first; the grid is Monday-first. */
function weekdayIndex(d: Date): number {
  return (d.getDay() + 6) % 7;
}

/** Bucketed on each sample's *local* clock — "when do I run hot" is a question about the user's day, not UTC's. */
function bucketize(samples: Sample[]): Bucket[] {
  const buckets: Bucket[] = Array.from({ length: CELLS }, () => ({ sum: 0, count: 0 }));
  for (const s of samples) {
    const at = new Date(s.timestamp);
    if (Number.isNaN(at.getTime()) || !Number.isFinite(s.bindingPct)) continue;
    const part = Math.min(PARTS.length - 1, Math.floor(at.getHours() / 6));
    const bucket = buckets[part * WEEKDAYS.length + weekdayIndex(at)];
    if (!bucket) continue;
    bucket.sum += s.bindingPct;
    bucket.count += 1;
  }
  return buckets;
}

/**
 * Weekday × part-of-day grid of average binding utilisation.
 *
 * A supporting element, not the hero: it answers "when am I habitually near
 * the limit", which no single figure on the screen can, and then gets out of
 * the way.
 */
export default function UsageHeatmap({ samples }: UsageHeatmapProps) {
  const captionId = useId();
  const buckets = bucketize(samples);
  const sampled = buckets.filter((b) => b.count > 0).length;

  // Only a sampled bucket can be hottest — an unsampled one has no average to
  // lose with.
  let hottest = -1;
  let hottestAvg = -1;
  buckets.forEach((b, i) => {
    if (b.count === 0) return;
    const avg = b.sum / b.count;
    if (avg > hottestAvg) {
      hottestAvg = avg;
      hottest = i;
    }
  });

  const part = hottest === -1 ? undefined : PARTS[Math.floor(hottest / WEEKDAYS.length)];
  const caption =
    hottest === -1
      ? "No samples recorded yet, so there is nothing to compare across the week."
      : `Hottest ${WEEKDAY_LONG[hottest % WEEKDAYS.length]} ${part?.label.toLowerCase()}, averaging ` +
        `${Math.round(hottestAvg)}%. ${sampled} of ${CELLS} slots sampled.`;

  return (
    <>
      <div className="heat" role="img" aria-labelledby={captionId}>
        <span className="heat-lab" />
        {WEEKDAYS.map((d) => (
          <span className="heat-lab" key={d}>
            {d}
          </span>
        ))}
        {PARTS.map((p, row) => (
          <HeatRow key={p.label} part={p} row={row} buckets={buckets} />
        ))}
      </div>

      <div className="heat-legend">
        <span>
          <i className="heat-cell is-empty" />
          no samples
        </span>
        <span>
          {[FLOOR, 0.45, 0.72, 1].map((o) => (
            <i className="heat-cell" key={o} style={{ opacity: o }} />
          ))}
          0% → 100% used
        </span>
        <span id={captionId}>{caption}</span>
      </div>
    </>
  );
}

function HeatRow({ part, row, buckets }: { part: (typeof PARTS)[number]; row: number; buckets: Bucket[] }) {
  return (
    <>
      {/* Full word: "Aft" and "Nig" are not words, and the column is now sized
          for the longest of them. */}
      <span className="heat-lab" title={`${part.label}, ${part.hours}`}>
        {part.label}
      </span>
      {WEEKDAYS.map((day, col) => {
        const bucket = buckets[row * WEEKDAYS.length + col];
        // Absence is a class, not an intensity: an unsampled slot is drawn as
        // a bare outline so it can never be misread as a low reading.
        if (!bucket || bucket.count === 0) {
          return <span className="heat-cell is-empty" key={day} title={`${day} ${part.label}: no samples`} />;
        }
        const avg = Math.max(0, Math.min(100, bucket.sum / bucket.count));
        return (
          <span
            className="heat-cell"
            key={day}
            style={{ opacity: FLOOR + (1 - FLOOR) * (avg / 100) }}
            title={`${day} ${part.label}: ${Math.round(avg)}% average over ${bucket.count} sample${
              bucket.count === 1 ? "" : "s"
            }`}
          />
        );
      })}
    </>
  );
}
