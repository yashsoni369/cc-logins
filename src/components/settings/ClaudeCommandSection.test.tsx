import { render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import * as api from "@/lib/api";
import ClaudeCommandSection, { vscodeSetting } from "@/components/settings/ClaudeCommandSection";
import type { CliStatus } from "@/types";

function status(overrides: Partial<CliStatus> = {}): CliStatus {
  return {
    health: { state: "notInstalled" },
    shimAvailable: true,
    binDir: "/home/u/.cc-logins/bin",
    commandPath: "/home/u/.cc-logins/bin/claude",
    launchers: [],
    ...overrides,
  };
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("vscodeSetting", () => {
  it("escapes Windows backslashes as JSON", () => {
    expect(vscodeSetting("C:\\Users\\u\\.cc-logins\\bin\\claude.exe")).toBe(
      '"claudeCode.claudeProcessWrapper": "C:\\\\Users\\\\u\\\\.cc-logins\\\\bin\\\\claude.exe"',
    );
  });
});

describe("ClaudeCommandSection", () => {
  it("explains itself without a backend", async () => {
    render(<ClaudeCommandSection />);
    expect(await screen.findByText("Available in the desktop app.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Install" })).not.toBeInTheDocument();
  });

  it("offers Install when not installed", async () => {
    vi.spyOn(api, "cliStatus").mockResolvedValue({ data: status(), live: true });
    render(<ClaudeCommandSection />);
    expect(await screen.findByRole("button", { name: "Install" })).toBeEnabled();
    expect(screen.queryByText("VS Code")).not.toBeInTheDocument();
  });

  it("shows launchers, the VS Code line and Uninstall once installed", async () => {
    vi.spyOn(api, "cliStatus").mockResolvedValue({
      data: status({
        health: { state: "installed" },
        launchers: [{ slug: "work", accountNumber: 2, command: "claude-work" }],
      }),
      live: true,
    });
    render(<ClaudeCommandSection />);
    expect(await screen.findByRole("button", { name: "Uninstall" })).toBeInTheDocument();
    expect(screen.getByText("claude-work")).toBeInTheDocument();
    expect(screen.getByText(vscodeSetting("/home/u/.cc-logins/bin/claude"))).toBeInTheDocument();
  });

  it("names what shadows the command", async () => {
    vi.spyOn(api, "cliStatus").mockResolvedValue({
      data: status({ health: { state: "shadowed", by: "/usr/local/bin/claude" } }),
      live: true,
    });
    render(<ClaudeCommandSection />);
    expect(await screen.findByText(/\/usr\/local\/bin\/claude comes first on PATH/)).toBeInTheDocument();
  });

  it("does not offer Install when this build lacks the shim", async () => {
    vi.spyOn(api, "cliStatus").mockResolvedValue({ data: status({ shimAvailable: false }), live: true });
    render(<ClaudeCommandSection />);
    expect(await screen.findByText(/doesn't include it/)).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Install" })).not.toBeInTheDocument();
  });
});
