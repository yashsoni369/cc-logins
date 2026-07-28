import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { daemonPhaseLabel, MainRecoveryBanner } from "./App";

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
