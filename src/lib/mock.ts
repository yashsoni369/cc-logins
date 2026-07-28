import type { Environment, Snapshot } from "../types";

/**
 * Static demo data for the Accounts screen, shaped exactly like the
 * `cswap --json` contract. Mirrors the four accounts shown in wireframe
 * screen 04 (naim / personal / work / spare).
 */
export const mockSnapshot: Snapshot = {
  schemaVersion: 1,
  activeAccountNumber: 1,
  environments: [
    {
      id: "native-windows",
      label: "Windows",
      path: "~/.claude",
      kind: "native",
      status: "live",
      accounts: [
        {
          number: 1,
          email: "naim@example.com",
          alias: "naim",
          active: true,
          usageStatus: "ok",
          usageFetchedAt: "2026-07-28T04:44:56Z",
          usageAgeSeconds: 4,
          usage: {
            fiveHour: {
              pct: 61,
              resetsAt: "2026-07-28T04:49:00Z",
              clock: "04:49",
              countdown: "4h 4m",
            },
            sevenDay: {
              pct: 13,
              resetsAt: "2026-08-01T00:00:00Z",
              clock: "Aug 1",
              countdown: "4d",
              expectedPct: 14,
              aheadOfPace: false,
              willLastToReset: true,
            },
          },
        },
        {
          number: 2,
          email: "person.two@example.com",
          alias: "personal",
          active: false,
          usageStatus: "ok",
          usageFetchedAt: "2026-07-28T04:44:48Z",
          usageAgeSeconds: 12,
          usage: {
            fiveHour: {
              pct: 4,
              resetsAt: "2026-07-28T02:31:00Z",
              clock: "02:31",
              countdown: "1h 46m",
            },
            sevenDay: {
              // Realistic pace: 31/39 ≈ 0.79×, matching the ratios actually
              // observed against the live usage API (all under 1.0, all
              // aheadOfPace: false) — the fixture used to claim 1.1× here,
              // a ratio that colored the same amber as a genuinely off-pace
              // account and never occurs in practice.
              pct: 31,
              resetsAt: "2026-08-01T00:00:00Z",
              clock: "Aug 1",
              countdown: "4d",
              expectedPct: 39,
              aheadOfPace: false,
              willLastToReset: true,
            },
          },
        },
        {
          number: 3,
          email: "work.acct@example.com",
          alias: "work",
          active: false,
          usageStatus: "ok",
          usageFetchedAt: "2026-07-28T04:44:52Z",
          usageAgeSeconds: 8,
          usage: {
            fiveHour: {
              pct: 78,
              resetsAt: "2026-07-28T00:44:00Z",
              clock: "00:44",
              countdown: "58m",
            },
            sevenDay: {
              pct: 82,
              resetsAt: "2026-08-01T00:00:00Z",
              clock: "Aug 1",
              countdown: "4d",
              expectedPct: 34,
              aheadOfPace: true,
              willLastToReset: false,
            },
          },
        },
        {
          number: 4,
          email: "setup-token-4@local",
          alias: "spare",
          active: false,
          usageStatus: "disabled",
          usageFetchedAt: "2026-07-28T04:41:00Z",
          usageAgeSeconds: 240,
          usage: {
            fiveHour: {
              pct: 0,
            },
            sevenDay: {
              pct: 3,
            },
          },
        },
      ],
    },
  ],
};

// ── History ─────────────────────────────────────────────────────────────

export type HistoryRangeId = "7d" | "30d" | "90d";

export interface HistoryRangeStats {
  weeklyAveragePct: number;
  accountsCounted: number;
  limitsHit: number;
  autoSwitches: number;
  busiestDay: string;
  busiestDayAvgPct: number;
}

export interface HistoryAccountSeries {
  id: string;
  label: string;
  /** Utilisation, 0..100, oldest first, one sample per day. */
  data: number[];
}

export interface HistoryRangeData {
  id: HistoryRangeId;
  /** Footer text for the stat row, e.g. "last 30 days". */
  label: string;
  /** Left x-axis label for every chart in this range, e.g. "30 days ago". */
  startLabel: string;
  stats: HistoryRangeStats;
  series: HistoryAccountSeries[];
}

/**
 * The exact 30-day burn-rate samples from wireframe screen 05 — copied
 * verbatim rather than randomised, so the default (30d) view matches the
 * design source precisely.
 */
const HISTORY_30D = {
  naim: [8, 12, 19, 31, 26, 22, 35, 44, 38, 29, 41, 52, 47, 39, 33, 45, 58, 64, 55, 48, 42, 37, 51, 63, 71, 66, 58, 49, 21, 13],
  personal: [22, 28, 24, 19, 33, 41, 47, 52, 44, 38, 31, 27, 35, 42, 49, 55, 61, 58, 51, 46, 39, 44, 52, 57, 63, 59, 54, 48, 44, 41],
  work: [12, 18, 27, 39, 48, 57, 66, 74, 81, 88, 92, 85, 71, 63, 58, 66, 74, 83, 91, 87, 79, 72, 68, 75, 84, 89, 94, 90, 86, 82],
  spare: [0, 0, 0, 0, 0, 0, 2, 4, 3, 1, 0, 0, 0, 0, 0, 0, 0, 1, 2, 2, 1, 0, 0, 0, 0, 0, 0, 1, 2, 3],
} as const;

const HISTORY_ACCOUNT_ORDER: Array<{ key: keyof typeof HISTORY_30D; label: string }> = [
  { key: "naim", label: "naim" },
  { key: "personal", label: "personal" },
  { key: "work", label: "work" },
  { key: "spare", label: "spare" },
];

/**
 * Deterministic filler for the days before the real 30-day sample, so the
 * 90-day view has continuity without hand-authoring 60 more data points.
 * Pure function of the index (no Math.random), so it renders identically on
 * every run.
 */
function extendBackwards(recent: readonly number[], extraDays: number): number[] {
  const anchor = recent[0] ?? 0;
  const filler: number[] = [];
  for (let i = 0; i < extraDays; i++) {
    const wave = Math.sin(i / 5) * 10 + Math.sin(i / 17) * 6;
    filler.push(Math.round(Math.max(0, Math.min(100, anchor + wave))));
  }
  return [...filler, ...recent];
}

const HISTORY_RANGE_META: Record<HistoryRangeId, { label: string; startLabel: string }> = {
  "7d": { label: "last 7 days", startLabel: "7 days ago" },
  "30d": { label: "last 30 days", startLabel: "30 days ago" },
  "90d": { label: "last 90 days", startLabel: "90 days ago" },
};

const HISTORY_RANGE_STATS: Record<HistoryRangeId, HistoryRangeStats> = {
  "7d": { weeklyAveragePct: 51, accountsCounted: 4, limitsHit: 1, autoSwitches: 4, busiestDay: "Tue", busiestDayAvgPct: 68 },
  "30d": { weeklyAveragePct: 54, accountsCounted: 4, limitsHit: 3, autoSwitches: 17, busiestDay: "Tue", busiestDayAvgPct: 71 },
  "90d": { weeklyAveragePct: 49, accountsCounted: 4, limitsHit: 9, autoSwitches: 46, busiestDay: "Tue", busiestDayAvgPct: 69 },
};

function buildHistoryRange(id: HistoryRangeId): HistoryRangeData {
  const series = HISTORY_ACCOUNT_ORDER.map(({ key, label }) => {
    const full30 = HISTORY_30D[key];
    const data = id === "7d" ? full30.slice(-7) : id === "30d" ? [...full30] : extendBackwards(full30, 60);
    return { id: key, label, data };
  });

  const meta = HISTORY_RANGE_META[id];
  return { id, label: meta.label, startLabel: meta.startLabel, stats: HISTORY_RANGE_STATS[id], series };
}

/** Small-multiples data for the History screen, one entry per 7d/30d/90d range. */
export const mockHistoryRanges: HistoryRangeData[] = (["7d", "30d", "90d"] as const).map(buildHistoryRange);

// ── Environments ────────────────────────────────────────────────────────

/**
 * Sample realms for the Environments screen: Windows native (live), a WSL
 * distro that is currently stopped (asleep — last-known values only, never
 * auto-polled because reading it would boot the VM), and a WSL distro with
 * no Claude Code install (ignored). Kept separate from `mockSnapshot` so the
 * Accounts screen's totals are unaffected — realms never merge.
 */
export const mockEnvironments: Environment[] = [
  {
    id: "native-windows",
    label: "Windows",
    path: "~/.claude",
    kind: "native",
    status: "live",
    accounts: [
      {
        number: 1,
        email: "naim@example.com",
        alias: "naim",
        active: true,
        usageStatus: "ok",
        usage: { fiveHour: { pct: 61 } },
      },
      {
        number: 2,
        email: "person.two@example.com",
        alias: "personal",
        active: false,
        usageStatus: "ok",
        usage: { fiveHour: { pct: 41 } },
      },
    ],
  },
  {
    id: "wsl-ubuntu",
    label: "WSL · Ubuntu",
    path: "~/.local/share/claude-swap",
    kind: "wsl",
    status: "asleep",
    lastSeenSeconds: 8040, // ~2h 14m
    accounts: [
      {
        number: 5,
        email: "work.wsl@example.com",
        alias: "work",
        active: true,
        usageStatus: "stale",
        usage: { fiveHour: { pct: 44 } },
      },
    ],
  },
  {
    id: "wsl-docker-desktop",
    label: "WSL · docker-desktop",
    path: "no Claude Code install",
    kind: "wsl",
    status: "ignored",
    accounts: [],
  },
];
