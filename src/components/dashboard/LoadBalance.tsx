import type { LoadRow } from "@/lib/dashboard";
import { useElementWidth } from "@/lib/useElementWidth";
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

/** Cell edge, in pixels, plus its gap. Held constant so a wider window buys
 *  more history rather than fatter blocks — stretched cells stop reading as a
 *  heatmap and start reading as a bar chart. */
const CELL = 14;
const GAP = 2;
/** Name gutter plus the gap after it, matching `.lb-row`'s grid. */
const GUTTER = 84 + 10;
/** Fewer than this is not a pattern; more than this outruns what is fetched. */
const MIN_DAYS = 14;
const MAX_DAYS = 180;

interface LoadBalanceProps {
  /** Cells for the widest span the screen fetched; this component shows the
   *  most recent slice that fits. */
  rows: LoadRow[];
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
export default function LoadBalance({ rows }: LoadBalanceProps) {
  const [wrapRef, width] = useElementWidth<HTMLDivElement>(1100);
  const available = Math.max(0, width - GUTTER);
  const fits = Math.floor((available + GAP) / (CELL + GAP));
  const longest = rows[0]?.cells.length ?? 0;
  const days = Math.max(MIN_DAYS, Math.min(fits, MAX_DAYS, longest || MIN_DAYS));

  // Newest days win when the window cannot show everything recorded.
  const shown = rows.map((row) => ({ ...row, cells: row.cells.slice(-days) }));
  const measured = shown.some((row) => row.cells.some((c) => c.peak != null));

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
          <div className="lb-scroll" ref={wrapRef}>
            <div className="lb-grid">
              {shown.map((row) => (
                <div className="lb-row" key={row.number}>
                  <span className="lb-lab" title={row.name}>
                    {row.name}
                  </span>
                  {/*
                    Fractional tracks, not fixed ones.

                    The day count above already targets a ~14px cell, so these
                    land near that on a normal window — but 1fr is what
                    guarantees they fit exactly. Fixed 14px tracks overflowed
                    the moment the window was narrower than the count assumed,
                    and answered a resize with a horizontal scrollbar.
                  */}
                  <div
                    className="lb-cells"
                    style={{ gridTemplateColumns: `repeat(${row.cells.length}, minmax(0, 1fr))` }}
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
