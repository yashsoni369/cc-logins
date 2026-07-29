import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import AccountsScreen from "./AccountsScreen";
import type { Snapshot } from "../types";

const snapshot: Snapshot = {
  schemaVersion: 1,
  environments: [
    {
      id: "native",
      label: "Native",
      path: "",
      kind: "native",
      status: "live",
      accounts: [
        {
          number: 2,
          email: "repair@example.com",
          alias: "Repair me",
          active: false,
          usageStatus: "reloginrequired",
        },
      ],
    },
  ],
};

describe("AccountsScreen dead credential rendering", () => {
  it("shows recovery guidance and prevents switching without calling it expired", () => {
    const onRelogin = vi.fn();
    render(
      <AccountsScreen
        snapshot={snapshot}
        onSwitch={vi.fn()}
        pendingAccount={null}
        switchError={null}
        onAddAccount={vi.fn()}
        pendingAddAccount={false}
        addAccountError={null}
        onAddToken={vi.fn().mockResolvedValue(undefined)}
        pendingAddToken={false}
        addTokenError={null}
        onInteractiveLogin={vi.fn()}
        pendingInteractiveLogin={false}
        interactiveLoginError={null}
        onRelogin={onRelogin}
        pendingReloginAccount={null}
        reloginError={null}
        onSetEnabled={vi.fn()}
        pendingEnableAccount={null}
        enableError={null}
        mutationInFlight={false}
        degraded={false}
      />,
    );

    expect(screen.getByText("Re-login required")).toBeInTheDocument();
    expect(screen.getByText(/sign in again to replace this account/i)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Switch" })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Re-login" }));
    expect(onRelogin).toHaveBeenCalledWith(2);
    expect(screen.queryByText(/expired/i)).not.toBeInTheDocument();
  });
});

/** Non-mutating props, so each test only spells out the handler it asserts on. */
function inertProps() {
  return {
    onSwitch: vi.fn(),
    pendingAccount: null,
    switchError: null,
    onAddAccount: vi.fn(),
    pendingAddAccount: false,
    addAccountError: null,
    onAddToken: vi.fn().mockResolvedValue(undefined),
    pendingAddToken: false,
    addTokenError: null,
    onInteractiveLogin: vi.fn(),
    pendingInteractiveLogin: false,
    interactiveLoginError: null,
    onRelogin: vi.fn(),
    pendingReloginAccount: null,
    reloginError: null,
    pendingEnableAccount: null,
    enableError: null,
    mutationInFlight: false,
    degraded: false,
  } as const;
}

/** One enabled and one held-out account, both with a fixed reset instant. */
const rotationSnapshot: Snapshot = {
  schemaVersion: 1,
  environments: [
    {
      id: "native",
      label: "Native",
      path: "",
      kind: "native",
      status: "live",
      accounts: [
        {
          number: 1,
          email: "work@example.com",
          alias: "Work",
          active: true,
          usageStatus: "ok",
          usage: {
            fiveHour: {
              pct: 40,
              resetsAt: "2025-01-01T16:21:00Z",
              // Deliberately wrong: the backend's cached strings are what the
              // countdown must *not* be read from while `resetsAt` parses.
              countdown: "9h 9m",
              clock: "20:39",
            },
          },
        },
        {
          number: 3,
          email: "spare@example.com",
          alias: "Spare",
          active: false,
          usageStatus: "disabled",
        },
      ],
    },
  ],
};

/** The reset instant is 4h 21m after this. */
const PINNED_NOW = new Date("2025-01-01T12:00:00Z");

describe("AccountsScreen reset countdown", () => {
  afterEach(() => vi.useRealTimers());

  it("renders a live countdown rather than the backend's absolute clock", () => {
    vi.useFakeTimers();
    vi.setSystemTime(PINNED_NOW);
    render(<AccountsScreen snapshot={rotationSnapshot} onSetEnabled={vi.fn()} {...inertProps()} />);

    // The "in" lives in the column header ("Resets in"), so the cell is bare.
    expect(screen.getByText("4h 21m")).toBeInTheDocument();
    // Neither the cached clock nor the cached (drifted) countdown wins while
    // `resetsAt` is parseable.
    expect(screen.queryByText("20:39")).not.toBeInTheDocument();
    expect(screen.queryByText("9h 9m")).not.toBeInTheDocument();
  });

  it("drops the Pace column but keeps a Resets in header", () => {
    render(<AccountsScreen snapshot={rotationSnapshot} onSetEnabled={vi.fn()} {...inertProps()} />);

    expect(screen.queryByRole("columnheader", { name: "Pace" })).not.toBeInTheDocument();
    expect(screen.getByRole("columnheader", { name: "Resets in" })).toBeInTheDocument();
  });
});

describe("AccountsScreen rotation switch", () => {
  it("reflects rotation membership as aria-checked", () => {
    render(<AccountsScreen snapshot={rotationSnapshot} onSetEnabled={vi.fn()} {...inertProps()} />);

    expect(screen.getByRole("switch", { name: /^Work / })).toHaveAttribute("aria-checked", "true");
    expect(screen.getByRole("switch", { name: /^Spare / })).toHaveAttribute("aria-checked", "false");
    // The pill stays: it is the shared status vocabulary, and reads without
    // parsing switch position.
    expect(screen.getByText(/held out/i)).toBeInTheDocument();
  });

  it("enables a held-out account without expanding its row", () => {
    const onSetEnabled = vi.fn();
    render(<AccountsScreen snapshot={rotationSnapshot} onSetEnabled={onSetEnabled} {...inertProps()} />);

    const toggle = screen.getByRole("switch", { name: /^Spare / });
    const row = toggle.closest("tr")!;
    fireEvent.click(toggle);

    expect(onSetEnabled).toHaveBeenCalledWith(3, true);
    // Regression guard for the click bubbling into the row's click-to-expand.
    expect(row).toHaveAttribute("aria-expanded", "false");
  });

  it("holds out an enabled account", () => {
    const onSetEnabled = vi.fn();
    render(<AccountsScreen snapshot={rotationSnapshot} onSetEnabled={onSetEnabled} {...inertProps()} />);

    fireEvent.click(screen.getByRole("switch", { name: /^Work / }));

    expect(onSetEnabled).toHaveBeenCalledWith(1, false);
  });
});
