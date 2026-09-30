import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import CommandPalette, { type PaletteCommand } from "@/components/CommandPalette";

function commands(run: () => void): PaletteCommand[] {
  return [
    { id: "switch-2", group: "Switch to", label: "Personal", meta: "23%", keywords: ["p@example.com"], run },
    { id: "go-history", group: "Go to", label: "History", run: vi.fn() },
  ];
}

describe("CommandPalette", () => {
  it("filters as you type and runs the highlighted command on Enter", () => {
    const run = vi.fn();
    const onClose = vi.fn();
    render(<CommandPalette open commands={commands(run)} onClose={onClose} />);

    const input = screen.getByRole("combobox");
    fireEvent.change(input, { target: { value: "pers" } });
    expect(screen.queryByText("History")).not.toBeInTheDocument();
    fireEvent.keyDown(input, { key: "Enter" });

    expect(onClose).toHaveBeenCalled();
    expect(run).toHaveBeenCalledTimes(1);
  });

  it("runs nothing just by opening", () => {
    const run = vi.fn();
    render(<CommandPalette open commands={commands(run)} onClose={vi.fn()} />);
    expect(run).not.toHaveBeenCalled();
  });
});
