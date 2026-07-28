import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import BurnRateChart from "./BurnRateChart";

describe("BurnRateChart", () => {
  it("rounds decimal history averages in visible and accessible labels", () => {
    render(
      <BurnRateChart
        series={{ id: "account-1", label: "Work", data: [null, 30.149253731343283] }}
        startLabel="7 days ago"
      />,
    );

    expect(screen.getByText("now 30%")).toBeInTheDocument();
    expect(screen.getByRole("img")).toHaveAccessibleName(
      "Work: 7 days ago to today, currently 30%",
    );
    expect(screen.queryByText(/30\.149253731343283/)).not.toBeInTheDocument();
  });
});
