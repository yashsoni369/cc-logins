import { describe, expect, it } from "vitest";

import {
  bindingUtilisation,
  bindingWindow,
  displayName,
  maskEmail,
  type Account,
  type Usage,
} from "./types";

function account(fields: Partial<Account>): Account {
  return { number: 1, email: "someone@example.com", active: false, usageStatus: "ok", ...fields };
}

/*
 * The popover prints a percentage and a reset time side by side and asks the
 * reader to treat them as one fact. That only holds if both come from the same
 * window, which is the invariant these cover.
 */
describe("bindingWindow", () => {
  it("returns the highest-utilised window, not the first one", () => {
    const usage: Usage = { fiveHour: { pct: 90 }, sevenDay: { pct: 40 } };
    expect(bindingWindow(usage)?.pct).toBe(90);
  });

  it("considers per-model weekly limits, which can bind before either headline window", () => {
    const usage: Usage = {
      fiveHour: { pct: 10 },
      sevenDay: { pct: 20 },
      scoped: [{ name: "Fable", pct: 97, resetsAt: "2026-08-01T00:00:00Z" }],
    };
    expect(bindingWindow(usage)?.pct).toBe(97);
    expect(bindingWindow(usage)?.resetsAt).toBe("2026-08-01T00:00:00Z");
  });

  it("carries the reset belonging to the window it picked", () => {
    const usage: Usage = {
      fiveHour: { pct: 90, resetsAt: "2026-07-28T14:00:00Z" },
      sevenDay: { pct: 40, resetsAt: "2026-08-02T00:00:00Z" },
    };
    // Pairing 90% with the seven-day clock would describe neither window.
    expect(bindingWindow(usage)?.resetsAt).toBe("2026-07-28T14:00:00Z");
  });

  it("is null when usage is unknown, never a zeroed window", () => {
    expect(bindingWindow(undefined)).toBeNull();
    expect(bindingWindow({})).toBeNull();
  });

  it("agrees with bindingUtilisation on every shape", () => {
    const shapes: Array<Usage | undefined> = [
      undefined,
      {},
      { fiveHour: { pct: 0 } },
      { sevenDay: { pct: 100 } },
      { fiveHour: { pct: 55 }, sevenDay: { pct: 55 } },
      { fiveHour: { pct: 12 }, sevenDay: { pct: 80 }, scoped: [{ name: "Opus", pct: 34 }] },
    ];
    for (const usage of shapes) {
      expect(bindingUtilisation(usage)).toBe(bindingWindow(usage)?.pct ?? null);
    }
  });
});

/*
 * People screenshot this app, so the local part is always masked. The domain is
 * not — it is the only thing left distinguishing two accounts, and it is also
 * the part with no length bound.
 */
describe("maskEmail", () => {
  it("keeps the first character and the whole domain", () => {
    expect(maskEmail("yash@gmail.com")).toBe("y•••@gmail.com");
  });

  it("does not shorten a long domain — truncation is the layout's job, not this one's", () => {
    const long = "yash@really-long-company-domain-name-with-subdomains.co.uk";
    expect(maskEmail(long)).toBe("y•••@really-long-company-domain-name-with-subdomains.co.uk");
  });

  it("passes through a value with no @ rather than mangling it", () => {
    expect(maskEmail("not-an-email")).toBe("not-an-email");
  });

  it("passes through a leading @, where there is no local part to mask", () => {
    // indexOf is 0, so slicing would produce "@•••@…" — the guard exists for this.
    expect(maskEmail("@example.com")).toBe("@example.com");
  });

  it("survives an empty string", () => {
    expect(maskEmail("")).toBe("");
  });
});

describe("displayName", () => {
  it("prefers an alias over the email", () => {
    expect(displayName(account({ alias: "work" }))).toBe("work");
  });

  it("trims an alias", () => {
    expect(displayName(account({ alias: "  work  " }))).toBe("work");
  });

  it("falls back to the masked email when the alias is blank or whitespace", () => {
    expect(displayName(account({ alias: "   ", email: "yash@gmail.com" }))).toBe("y•••@gmail.com");
    expect(displayName(account({ alias: "", email: "yash@gmail.com" }))).toBe("y•••@gmail.com");
  });

  it("never invents a name it was not given", () => {
    // Documents current behaviour: an account with no alias and no email
    // renders blank rather than a placeholder. Worth revisiting if the backend
    // ever emits one.
    expect(displayName(account({ email: "" }))).toBe("");
  });
});
