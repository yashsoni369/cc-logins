/**
 * Carries `Settings.displayMode` down to every meter, the same way
 * `clockFormat.tsx` carries the clock preference. A subtree with no provider
 * reads "used", which is how the app has always shown quota.
 */

import { createContext, useContext, type ReactNode } from "react";

export type DisplayMode = "used" | "left";

const DisplayModeContext = createContext<DisplayMode>("used");

export function DisplayModeProvider({ value, children }: { value: DisplayMode; children: ReactNode }) {
  return <DisplayModeContext.Provider value={value}>{children}</DisplayModeContext.Provider>;
}

export function useDisplayMode(): DisplayMode {
  return useContext(DisplayModeContext);
}
