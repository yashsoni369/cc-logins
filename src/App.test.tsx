import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { daemonPhaseLabel, describeInteractiveLoginError, MainRecoveryBanner } from "./App";
import { IpcError } from "./lib/api";

describe("main-window daemon state", () => {
  it("labels authoritative phases instead of claiming auto-switch is always running", () => {
    expect(daemonPhaseLabel({ kind: "disabled" }, "most-headroom")).toBe("Off");
    expect(daemonPhaseLabel({ kind: "paused", until: "2026-07-29T12:00:00Z" }, "most-headroom"))
      .toBe("Paused");
    expect(daemonPhaseLabel({ kind: "monitoring" }, "next-available")).toBe("Running · next");
    expect(daemonPhaseLabel({ kind: "recoveryRequired", detail: "repair needed" }, "most-headroom"))
      .toBe("Recovery required");
  });

  it("shows recovery detail and explains that account changes are blocked", () => {
    render(<MainRecoveryBanner phase={{ kind: "recoveryRequired", detail: "journal requires repair" }} />);

    expect(screen.getByRole("alert")).toHaveTextContent("Recovery required");
    expect(screen.getByRole("alert")).toHaveTextContent("journal requires repair");
    expect(screen.getByRole("alert")).toHaveTextContent("Account changes are disabled");
  });
});

describe("interactive login error messages", () => {
  // The backend names the directories it actually searched and how to override
  // the lookup. This branch used to discard `detail` and always render the
  // static sentence, which made every backend message improvement invisible.
  it("returns the detail when prerequisiteMissing carries one", () => {
    const err = new IpcError("prerequisiteMissing", "claude binary not found in: /usr/bin, /usr/local/bin, ~/.local/bin");
    expect(describeInteractiveLoginError(err)).toBe("claude binary not found in: /usr/bin, /usr/local/bin, ~/.local/bin");
  });

  it("returns the static fallback when detail is absent", () => {
    const err = new IpcError("prerequisiteMissing");
    expect(describeInteractiveLoginError(err)).toBe(
      "Claude Code isn't installed, or the `claude` command isn't on PATH. Install it, then try again."
    );
  });

  it("returns null for a cancelled login", () => {
    const err = new IpcError("cancelled");
    expect(describeInteractiveLoginError(err)).toBeNull();
  });
});
