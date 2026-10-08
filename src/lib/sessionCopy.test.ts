import { describe, expect, it } from "vitest";

import { ageLabel, type Account } from "@/types";
import {
  autoPickedNotice,
  currentLabel,
  freshnessLabel,
  pickedMessage,
  profileProblem,
  setLiveSwap,
  useLabel,
  useLongLabel,
} from "@/lib/sessionCopy";

const legacy: Account = { number: 1, email: "a@x.com", active: true, usageStatus: "ok" };
const profile: Account = {
  ...legacy,
  number: 2,
  profile: { isDefault: false, launcher: "work", state: "ready" },
};

describe("sessionCopy", () => {
  it("never describes a profile account as swapped in", () => {
    expect(currentLabel(legacy)).toBe("in use");
    expect(currentLabel(profile)).toBe("new sessions");
    expect(useLabel(legacy, false)).toBe("Switch");
    expect(useLabel(profile, false)).toBe("Use");
    expect(useLabel(profile, true)).toBe("Selecting…");
    expect(useLongLabel(profile, "Work")).toBe("Use for new sessions");
    expect(pickedMessage(profile, "Work")).toMatch(/Running sessions keep their account/);
    expect(pickedMessage(legacy, "Main")).toBe("Switched to Main");
  });

  it("words a profile account as swapped in when running sessions follow the pick", () => {
    setLiveSwap(true);
    try {
      expect(currentLabel(profile)).toBe("in use");
      expect(useLabel(profile, false)).toBe("Switch");
      expect(useLongLabel(profile, "Work")).toBe("Switch to Work");
      expect(pickedMessage(profile, "Work")).toBe("Switched to Work. Running sessions use it on their next request.");
      expect(autoPickedNotice(profile, "Work", "Main").title).toBe("Switched to Work");
    } finally {
      setLiveSwap(false);
    }
  });

  it("words the auto-switch notice by account kind", () => {
    expect(autoPickedNotice(profile, "Work", "Main").title).toBe("New sessions will use Work");
    expect(autoPickedNotice(legacy, "Main", "Work").title).toBe("Switched to Main");
    expect(autoPickedNotice(undefined, "X", "Y").title).toBe("Switched to X");
  });

  it("labels idle and reset readings", () => {
    expect(freshnessLabel(profile, ageLabel)).toBeNull();
    expect(freshnessLabel({ ...profile, usageFreshness: { kind: "live" } }, ageLabel)).toBeNull();
    expect(
      freshnessLabel({ ...profile, usageFreshness: { kind: "lastKnown", ageSeconds: 3 * 3600 } }, ageLabel),
    ).toBe("idle · 3h old");
    expect(freshnessLabel({ ...profile, usageFreshness: { kind: "reset", ageSeconds: 10 } }, ageLabel)).toBe(
      "reset since",
    );
  });

  it("names profile problems", () => {
    expect(profileProblem(profile)).toBeNull();
    expect(profileProblem(legacy)).toBeNull();
    expect(profileProblem({ ...profile, profile: { isDefault: false, state: "loginRequired" } })).toBe("Signed out");
  });
});
