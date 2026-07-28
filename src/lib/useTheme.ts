/**
 * Owns one window's theme: loads the persisted choice, applies it to
 * `document.documentElement`, and keeps `resolvedTheme` current when the OS
 * theme changes while `"system"` is selected.
 *
 * This app has two separate browsing contexts — the dashboard window
 * (`src/main.tsx` → `src/App.tsx`) and the tray popover
 * (`src/popover.tsx` → `src/components/PopoverPanel.tsx`) — each with its
 * own `documentElement`. Each must mount its own instance of this hook so
 * each applies the theme to its own document; they don't fight each other
 * because both ultimately read the one persisted `Settings.theme` and apply
 * the same deterministic mapping.
 *
 * ## Mechanism
 *
 * `src/styles/tokens.css` defines the full palette for `:root` (dark
 * default), `@media (prefers-color-scheme: light)`, and both
 * `:root[data-theme="light"]` / `:root[data-theme="dark"]` explicit
 * overrides. So: an explicit theme sets `data-theme` to force it in either
 * direction; `"system"` removes the attribute entirely so the media query
 * governs. Do not change any token value from here.
 *
 * ## No flash of the wrong theme
 *
 * The persisted theme only arrives after the settings owner hydrates, but
 * the very first paint can't wait for that. So every time the resolved
 * theme is applied, it is also cached in `localStorage` — a render-time
 * cache only, never the source of truth — which a small inline script in
 * `index.html` and `popover.html` reads synchronously before the app bundle
 * loads, so the first frame already has the (likely) right theme applied.
 * See `RESOLVED_THEME_STORAGE_KEY` below; the key must match those scripts.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { IpcError } from "@/lib/api";
import type { UseSettingsResult } from "@/lib/useSettings";
import type { Settings } from "@/types";

export type Theme = Settings["theme"];
export type ResolvedTheme = "day" | "night";

/** Must match the key read by the inline scripts in index.html / popover.html. */
export const RESOLVED_THEME_STORAGE_KEY = "cc-logins:resolved-theme";

function systemPrefersDark(): boolean {
  return typeof window !== "undefined" && typeof window.matchMedia === "function"
    ? window.matchMedia("(prefers-color-scheme: dark)").matches
    : false;
}

/** Concrete theme for a preference, resolving "system" against the OS right now. */
function resolve(theme: Theme): ResolvedTheme {
  if (theme === "day" || theme === "night") return theme;
  return systemPrefersDark() ? "night" : "day";
}

/**
 * Sets/removes `data-theme` on `<html>` and refreshes the render-time cache.
 * `localStorage` writes are best-effort: a full disk or blocked storage must
 * not break theming, only the flash-prevention optimisation.
 */
function applyTheme(theme: Theme, resolved: ResolvedTheme): void {
  const root = document.documentElement;
  if (theme === "system") {
    root.removeAttribute("data-theme");
  } else {
    root.setAttribute("data-theme", theme === "day" ? "light" : "dark");
  }
  try {
    window.localStorage.setItem(RESOLVED_THEME_STORAGE_KEY, resolved);
  } catch {
    // Best-effort cache only.
  }
}

export interface UseThemeResult {
  /** The persisted preference. */
  theme: Theme;
  /** Always concrete ("day" | "night"), for anything that needs to branch. */
  resolvedTheme: ResolvedTheme;
  /**
   * Applies instantly (before the backend round-trip) and then persists.
   * Only ever call this from a real user interaction — never from an
   * effect, timer, or mount path.
   */
  setTheme: (theme: Theme) => void;
  /**
   * Set when the most recent persist failed. The visual change made by
   * `setTheme` is kept regardless — reverting something the user can
   * plainly see happened would be its own kind of lie.
   */
  error: string | null;
}

export function useTheme(runtime: UseSettingsResult): UseThemeResult {
  const [theme, setThemeState] = useState<Theme>("system");
  const [resolvedTheme, setResolvedTheme] = useState<ResolvedTheme>(() => resolve("system"));
  const [error, setError] = useState<string | null>(null);

  // Read from timers/callbacks instead of state so they never see a value
  // stale-captured at the point the callback was created.
  const themeRef = useRef<Theme>("system");
  const mounted = useRef(true);

  // Apply every newly confirmed revision. Including the revision in the
  // dependency is intentional: a conflict can rehydrate the same theme value
  // after an optimistic local change, and that confirmation must restore it.
  useEffect(() => {
    mounted.current = true;
    const confirmed = runtime.settings?.theme;
    if (confirmed) {
      themeRef.current = confirmed;
      setThemeState(confirmed);
      const resolved = resolve(confirmed);
      setResolvedTheme(resolved);
      applyTheme(confirmed, resolved);
    }
    return () => {
      mounted.current = false;
    };
  }, [runtime.settings?.theme, runtime.snapshot?.revision]);

  // React live to the OS theme changing while the app is open — only
  // matters while "system" is selected, but the resolved value must stay
  // current regardless of which theme is active when the listener fires.
  useEffect(() => {
    if (typeof window.matchMedia !== "function") return;
    const mql = window.matchMedia("(prefers-color-scheme: dark)");
    const onChange = () => {
      if (themeRef.current !== "system") return;
      const resolved = resolve("system");
      setResolvedTheme(resolved);
      applyTheme("system", resolved);
    };
    mql.addEventListener("change", onChange);
    return () => mql.removeEventListener("change", onChange);
  }, []);

  const setTheme = useCallback(
    (next: Theme) => {
      themeRef.current = next;
      setThemeState(next);
      const resolved = resolve(next);
      setResolvedTheme(resolved);
      applyTheme(next, resolved); // instant — do not wait on the backend round-trip
      setError(null);

      void runtime.update({ theme: next }).catch((err: unknown) => {
        if (!mounted.current) return;
        setError(err instanceof IpcError ? err.message : "Couldn't save theme.");
      });
    },
    [runtime.update],
  );

  return { theme, resolvedTheme, setTheme, error };
}
