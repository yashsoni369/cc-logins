import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

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
