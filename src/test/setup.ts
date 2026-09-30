import "@testing-library/jest-dom/vitest";

import { clearMocks } from "@tauri-apps/api/mocks";
import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

// jsdom does not implement the modal half of <dialog>. Model just enough of
// it — the `open` attribute — for components built on `showModal()`.
if (typeof HTMLDialogElement !== "undefined" && !HTMLDialogElement.prototype.showModal) {
  HTMLDialogElement.prototype.showModal = function showModal(this: HTMLDialogElement) {
    this.setAttribute("open", "");
  };
  HTMLDialogElement.prototype.close = function close(this: HTMLDialogElement) {
    this.removeAttribute("open");
  };
}

// cmdk measures its list with ResizeObserver and scrolls the active item into
// view; jsdom has neither.
if (typeof window !== "undefined") {
  if (!("ResizeObserver" in window)) {
    (window as unknown as { ResizeObserver: unknown }).ResizeObserver = class {
      observe() {}
      unobserve() {}
      disconnect() {}
    };
  }
  if (!Element.prototype.scrollIntoView) Element.prototype.scrollIntoView = () => {};
}

afterEach(() => {
  cleanup();
  clearMocks();
});
