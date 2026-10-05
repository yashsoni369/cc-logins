import { act, fireEvent, render, renderHook, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import * as api from "@/lib/api";
import { ClaudeCommandBanner, ClaudeCommandDialog } from "@/components/ClaudeCommandPrereq";
import { useCliStatus } from "@/lib/useCliStatus";
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

describe("ClaudeCommandBanner", () => {
  it("offers Install when the command is missing", () => {
    const onInstall = vi.fn();
    render(
      <ClaudeCommandBanner shadowedBy={null} installing={false} error={null} onInstall={onInstall} onOpenSettings={vi.fn()} />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Install" }));
    expect(onInstall).toHaveBeenCalled();
  });

  it("names what shadows the command and points to Settings", () => {
    const onOpenSettings = vi.fn();
    render(
      <ClaudeCommandBanner
        shadowedBy="/usr/local/bin/claude"
        installing={false}
        error={null}
        onInstall={vi.fn()}
        onOpenSettings={onOpenSettings}
      />,
    );
    expect(screen.getByText(/\/usr\/local\/bin\/claude/)).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "How to fix" }));
    expect(onOpenSettings).toHaveBeenCalled();
  });
});

describe("ClaudeCommandDialog", () => {
  it("installs and uses, or backs out", () => {
    const onInstallAndUse = vi.fn();
    const onCancel = vi.fn();
    render(
      <ClaudeCommandDialog
        open
        accountName="Work"
        installing={false}
        error={null}
        onInstallAndUse={onInstallAndUse}
        onCancel={onCancel}
      />,
    );
    fireEvent.click(screen.getByRole("button", { name: "Install and use Work" }));
    expect(onInstallAndUse).toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Not now" }));
    expect(onCancel).toHaveBeenCalled();
  });
});

describe("useCliStatus", () => {
  it("treats a missing command in a build that ships the shim as a missing prerequisite", async () => {
    vi.spyOn(api, "cliStatus").mockResolvedValue({ data: status(), live: true });
    const { result } = renderHook(() => useCliStatus());
    await waitFor(() => expect(result.current.status).toBeDefined());
    expect(result.current.missing).toBe(true);
  });

  it("never blocks a build without the shim", async () => {
    vi.spyOn(api, "cliStatus").mockResolvedValue({ data: status({ shimAvailable: false }), live: true });
    const { result } = renderHook(() => useCliStatus());
    await waitFor(() => expect(result.current.status).toBeDefined());
    expect(result.current.missing).toBe(false);
  });

  it("clears the prerequisite after installing", async () => {
    vi.spyOn(api, "cliStatus").mockResolvedValue({ data: status(), live: true });
    vi.spyOn(api, "installCli").mockResolvedValue(status({ health: { state: "installed" } }));
    const { result } = renderHook(() => useCliStatus());
    await waitFor(() => expect(result.current.missing).toBe(true));
    let ok = false;
    await act(async () => {
      ok = await result.current.install();
    });
    expect(ok).toBe(true);
    expect(result.current.missing).toBe(false);
  });

  it("reports shadowing", async () => {
    vi.spyOn(api, "cliStatus").mockResolvedValue({
      data: status({ health: { state: "shadowed", by: "C:\\Tools\\claude.cmd" } }),
      live: true,
    });
    const { result } = renderHook(() => useCliStatus());
    await waitFor(() => expect(result.current.shadowedBy).toBe("C:\\Tools\\claude.cmd"));
    expect(result.current.missing).toBe(false);
  });
});
