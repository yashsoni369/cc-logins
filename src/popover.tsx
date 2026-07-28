/**
 * Entry point for the tray popover window (`popover.html`).
 *
 * A separate React root from `main.tsx` because this is a separate Tauri
 * window (label "popover") with its own webview — Tauri does not let two
 * windows share a single page. It imports the same `app.css` as the main
 * window so `PopoverPanel` can reuse every existing primitive class.
 */

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import PopoverPanel from "./components/PopoverPanel";
import "./styles/app.css";

const container = document.getElementById("root");
if (!container) {
  throw new Error("#root element not found");
}

createRoot(container).render(
  <StrictMode>
    <PopoverPanel />
  </StrictMode>,
);
