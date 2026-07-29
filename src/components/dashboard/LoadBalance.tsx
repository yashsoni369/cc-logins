import type { LoadRow } from "@/lib/dashboard";
import { quotaState } from "@/types";

/** Legend stops, chosen to straddle the caution and danger thresholds. */
const KEY_STOPS = [12, 40, 70, 88, 99];

/**
 * A cell's fill: the quota-state hue at an intensity tracking the reading.
 *
 * `color-mix` against the surface rather than opacity, so a light cell stays
 * opaque and does not pick up whatever sits behind the grid.
 */
function fillFor(peak: number | null): string {
  if (peak == null) return "var(--surface)";
  const state = quotaState(peak);
  const weight = Math.round(20 + (Math.max(0, Math.min(100, peak)) / 100) * 80);
  return `color-mix(in oklch, var(--${state}-fill) ${weight}%, var(--surface))`;
}

interface LoadBalanceProps {
  rows: LoadRow[];
  days: number;
}

/**
 * Each account's daily peak, one row per account.
 *
 * This is the only view that answers whether the rotation is actually
 * spreading work or just draining one account — a question the per-account
 * charts cannot answer, because each is drawn in isolation. An unmeasured day
 * is left at surface tone: the app was not running, and a gap in recording
 * must not read as a quiet day.
 */
export default function LoadBalance({ rows, days }: LoadBalanceProps) {
  const measured = rows.some((row) => row.cells.some((c) => c.peak != null));

  return (
    <section className="band">
      <div className="band-head">
        <h2>Load balance</h2>
        <span className="sub">daily peak per account, last {days} days</span>
      </div>

      {!measured ? (
        <div className="empty">
          <p>Nothing recorded yet — a day appears here once the app has measured it.</p>
        </div>
      ) : (
        <>
          <div className="lb-scroll">
            <div className="lb-grid">
              {rows.map((row) => (
                <div className="lb-row" key={row.number}>
                  <span className="lb-lab" title={row.name}>
                    {row.name}
                  </span>
                  <div
                    className="lb-cells"
                    style={{ gridTemplateColumns: `repeat(${row.cells.length}, minmax(4px, 1fr))` }}
                  >
                    {row.cells.map((cell) => (
                      <div
                        key={cell.day}
                        className="lb-c"
                        style={{ background: fillFor(cell.peak) }}
                        title={
                          cell.peak == null
                            ? `${row.name} · ${cell.day} — no reading`
                            : `${row.name} · ${cell.day} — peaked at ${Math.round(cell.peak)}% over ` +
                              `${cell.sampleCount} reading${cell.sampleCount === 1 ? "" : "s"}`
                        }
                      />
                    ))}
                  </div>
                </div>
              ))}
            </div>
          </div>

          <div className="lb-foot">
            <span>idle</span>
            <span className="lb-key" aria-hidden="true">
              {KEY_STOPS.map((v) => (
                <i key={v} style={{ background: fillFor(v) }} />
              ))}
            </span>
            <span>saturated</span>
            <span className="lb-foot-note">blank cells are days with no reading</span>
          </div>
        </>
      )}
    </section>
  );
}
