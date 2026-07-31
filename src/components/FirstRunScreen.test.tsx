import { render, screen, within } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import FirstRunScreen from "@/components/FirstRunScreen";

/*
 * The bug this file exists for: someone with a Claude subscription, signed
 * into Claude Code, opened the app and was told "No accounts yet" — which
 * reads as "you have no Claude account" rather than "this app is not tracking
 * one". The screen already had the one-click button; it just never said it had
 * found anything, and buried it under the sign-in flow.
 */
describe("FirstRunScreen", () => {
  const props = { onAction: vi.fn(), pending: null, error: null };

  it("never claims the user has no Claude account", () => {
    // The old heading. It is about *this app's* vault, and said something much
    // broader than it meant.
    render(<FirstRunScreen {...props} loginPresent={false} />);
    expect(screen.queryByText("No accounts yet")).not.toBeInTheDocument();
    expect(screen.getByText("Not tracking any accounts yet")).toBeInTheDocument();
  });

  it("says so when a login was found, and leads with adding it", () => {
    render(<FirstRunScreen {...props} loginPresent />);

    expect(screen.getByText("Found your Claude Code login")).toBeInTheDocument();
    const add = screen.getByRole("button", { name: /Add my current login/ });
    expect(add).toHaveClass("primary");
    expect(screen.getByRole("button", { name: /Sign in/ })).not.toHaveClass("primary");
  });

  it("puts adding the existing login first when one was found", () => {
    const { container } = render(<FirstRunScreen {...props} loginPresent />);
    const steps = [...container.querySelectorAll(".step")];
    expect(within(steps[0] as HTMLElement).getByRole("button")).toHaveAccessibleName(
      /Add my current login/,
    );
  });

  /*
   * macOS keeps the active credential in the Keychain, and querying another
   * application's item is what raises a system prompt — so the probe returns
   * undefined there rather than asking. Undetermined has to behave like
   * "possibly", or every Mac user gets told they have no login.
   */
  it("treats an undetermined probe as possibly-signed-in, not as no", () => {
    render(<FirstRunScreen {...props} loginPresent={undefined} />);

    // No confident claim either way...
    expect(screen.queryByText("Found your Claude Code login")).not.toBeInTheDocument();
    // ...but the one-click path is offered first and prominently.
    const add = screen.getByRole("button", { name: /Add my current login/ });
    expect(add).toHaveClass("primary");
  });

  it("leads with signing in only when there is definitely no login", () => {
    const { container } = render(<FirstRunScreen {...props} loginPresent={false} />);
    const steps = [...container.querySelectorAll(".step")];
    expect(within(steps[0] as HTMLElement).getByRole("button")).toHaveAccessibleName(/Sign in/);
    expect(screen.getByRole("button", { name: /Sign in/ })).toHaveClass("primary");
  });

  it("offers both routes in every state", () => {
    for (const loginPresent of [true, false, undefined]) {
      const { unmount } = render(<FirstRunScreen {...props} loginPresent={loginPresent} />);
      expect(screen.getByRole("button", { name: /Add my current login/ })).toBeInTheDocument();
      expect(screen.getByRole("button", { name: /Sign in/ })).toBeInTheDocument();
      unmount();
    }
  });

  it("still shows no example account, whatever it detected", () => {
    // An earlier version displayed a hardcoded address as though it had found
    // one. Detecting a login must not become licence to invent its identity —
    // the probe deliberately never reads who it belongs to.
    render(<FirstRunScreen {...props} loginPresent />);
    expect(screen.queryByText(/@/)).not.toBeInTheDocument();
  });
});
