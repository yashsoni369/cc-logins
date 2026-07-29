/**
 * Carries the persisted `Settings.clockFormat` down to whatever renders a
 * time, so no component has to reach for the settings owner just to format an
 * hour.
 *
 * The dashboard window and the tray popover mount separately, so a subtree
 * may well have no provider above it. That degrades to `"system"` — the OS
 * locale decides — rather than throwing; a missing preference is not worth a
 * blank window.
 */

import { createContext, useContext, type ReactNode } from "react";

import type { ClockFormat } from "@/lib/time";

const ClockFormatContext = createContext<ClockFormat>("system");

export function ClockFormatProvider({
  value,
  children,
}: {
  value: ClockFormat;
  children: ReactNode;
}) {
  return <ClockFormatContext.Provider value={value}>{children}</ClockFormatContext.Provider>;
}

export function useClockFormat(): ClockFormat {
  return useContext(ClockFormatContext);
}
