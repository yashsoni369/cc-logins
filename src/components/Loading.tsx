/**
 * The two shapes of "still fetching", replacing the bare `Loading…` text.
 *
 * The animated mark is decorative and lives entirely in CSS, so the label stays
 * in the DOM: `.loading` replaces it visually, but a screen reader still reads
 * it and `role="status"` announces it when it appears.
 */

/** Inline, for a value that is loading inside a line of existing UI. */
export function Loading({ label = "Loading" }: { label?: string }) {
  return (
    <span className="loading" role="status">
      {label}
    </span>
  );
}

/** Centred in the `.pane` it fills — first paint, when there is nothing else to show. */
export function LoadingPane({ label = "Loading" }: { label?: string }) {
  return (
    <div className="loading-pane" role="status">
      <span className="loading">{label}</span>
    </div>
  );
}
