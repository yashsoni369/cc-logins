import { describe, expect, it } from "vitest";

import { noticeState, quotaNotices, type NoticeState } from "@/lib/notifications";
import type { Account, Snapshot } from "@/types";

const all = { notifyOnSwitch: true, notifyOnExhausted: true, notifyOnExpiry: true };

function snap(accounts: Account[]): Snapshot {
  return {
    schemaVersion: 1,
    environments: [{ id: "native", label: "Native", path: "", kind: "native", status: "live", accounts }],
  };
}

const work: Account = { number: 1, email: "w@example.com", alias: "Work", active: true, usageStatus: "ok" };
const home: Account = { number: 2, email: "h@example.com", alias: "Home", active: false, usageStatus: "ok" };

function state(active: number | null, phase: NoticeState["phaseKind"], relogin: number[] = []): NoticeState {
  return { activeNumber: active, phaseKind: phase, relogin: new Set(relogin) };
}

describe("quotaNotices", () => {
  it("says nothing on the first observation", () => {
    expect(quotaNotices(null, state(1, "monitoring"), [work, home], undefined, all, "24h")).toEqual([]);
  });

  it("announces an automatic switch", () => {
    const notices = quotaNotices(state(1, "switching"), state(2, "monitoring"), [work, home], undefined, all, "24h");
    expect(notices).toHaveLength(1);
    expect(notices[0]!.title).toBe("Switched to Home");
  });

  it("does not announce a manual switch", () => {
    expect(quotaNotices(state(1, "monitoring"), state(2, "monitoring"), [work, home], undefined, all, "24h")).toEqual([]);
  });

  it("announces exhaustion once, not on every poll", () => {
    const phase = { kind: "exhausted" as const, earliestReset: null };
    expect(quotaNotices(state(1, "monitoring"), state(1, "exhausted"), [work], phase, all, "24h")).toHaveLength(1);
    expect(quotaNotices(state(1, "exhausted"), state(1, "exhausted"), [work], phase, all, "24h")).toEqual([]);
  });

  it("announces a newly rejected login once", () => {
    expect(quotaNotices(state(1, "monitoring"), state(1, "monitoring", [2]), [work, home], undefined, all, "24h")[0]!.title).toBe(
      "Home needs a fresh sign-in",
    );
    expect(quotaNotices(state(1, "monitoring", [2]), state(1, "monitoring", [2]), [work, home], undefined, all, "24h")).toEqual([]);
  });

  it("respects each setting", () => {
    const none = { notifyOnSwitch: false, notifyOnExhausted: false, notifyOnExpiry: false };
    expect(quotaNotices(state(1, "switching"), state(2, "exhausted", [1]), [work, home], undefined, none, "24h")).toEqual([]);
  });
});

describe("noticeState", () => {
  it("reads the active account and rejected logins from a snapshot", () => {
    const s = noticeState(snap([work, { ...home, usageStatus: "reloginrequired" }]), { kind: "monitoring" });
    expect(s.activeNumber).toBe(1);
    expect([...s.relogin]).toEqual([2]);
    expect(s.phaseKind).toBe("monitoring");
  });
});
