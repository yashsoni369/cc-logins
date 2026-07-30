import { useEffect, useRef, useState, type RefObject } from "react";

/**
 * An element's rendered width, in CSS pixels.
 *
 * Every chart on the dashboard draws at one SVG user unit per pixel so that
 * its type never scales with its container. A fixed viewBox stretched to fill
 * a wide pane enlarges every label along with the drawing — `ResetStagger`
 * documents having hit exactly that, with labels rendering three times over
 * and colliding with the bands. Measuring the box and drawing to it keeps
 * 11px type at 11px whatever the window does.
 *
 * Returns `fallback` until the first measurement lands, and permanently in
 * environments with no `ResizeObserver` — jsdom among them, so a test renders
 * a sane fixed-width chart rather than a zero-width one.
 */
export function useElementWidth<T extends HTMLElement>(
  fallback: number,
): [RefObject<T | null>, number] {
  const ref = useRef<T | null>(null);
  const [width, setWidth] = useState(fallback);

  useEffect(() => {
    const node = ref.current;
    if (!node || typeof ResizeObserver === "undefined") return;

    const observer = new ResizeObserver((entries) => {
      const measured = entries[0]?.contentRect.width;
      // A collapsed or hidden container reports 0; keeping the previous width
      // stops the drawing from folding in on itself while it is off screen.
      if (measured && measured > 0) setWidth(Math.round(measured));
    });
    observer.observe(node);
    return () => observer.disconnect();
  }, []);

  return [ref, width];
}
