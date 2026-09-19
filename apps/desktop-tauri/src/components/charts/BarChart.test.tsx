import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { BarChart } from "./BarChart";

describe("BarChart calendar slots", () => {
  it("keeps unknown and known-zero slots distinct", () => {
    const { container } = render(
      <BarChart
        data={[
          { label: "unknown", value: null },
          { label: "zero", value: 0 },
          { label: "known", value: 2 },
        ]}
        ariaLabel="history"
        animations={false}
      />,
    );
    const bars = container.querySelectorAll(".chart__bar");
    expect(bars).toHaveLength(3);
    expect(bars[0]).toHaveAttribute("opacity", "0");
    expect(bars[1]).toHaveAttribute("opacity", "0.25");
    expect(container).toHaveTextContent("unknown");
    expect(container).toHaveTextContent("zero: 0.00");
  });

  it("keeps full endpoint dates in the axis", () => {
    const { container } = render(
      <BarChart
        data={[
          { label: "2026-09-01", value: 2 },
          { label: "2026-09-30", value: 3 },
        ]}
        ariaLabel="history"
        animations={false}
      />,
    );

    const labels = container.querySelectorAll(".chart__axis > span");
    expect(labels).toHaveLength(2);
    expect(labels[0]).toHaveTextContent("2026-09-01");
    expect(labels[1]).toHaveTextContent("2026-09-30");
    expect((labels[0] as HTMLElement).style.left).toBe("87.5px");
    expect((labels[1] as HTMLElement).style.left).toBe("192.5px");
    expect(container.querySelector(".chart__axis-max")).toBeNull();
    expect(labels[0]).toHaveClass("chart__axis-start");
    expect(labels[1]).toHaveClass("chart__axis-end");
    expect((labels[0] as HTMLElement).style.transform).toBe("");
    expect((labels[1] as HTMLElement).style.transform).toBe("");
  });
});
