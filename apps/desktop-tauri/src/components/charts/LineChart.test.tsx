import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { LineChart } from "./LineChart";

describe("LineChart unknown values", () => {
  it("renders gaps for unknown values while preserving known zero", () => {
    const { container } = render(
      <LineChart
        data={[
          { label: "2026-09-01", value: 1 },
          { label: "2026-09-02", value: 0 },
          { label: "2026-09-03", value: null },
          { label: "2026-09-04", value: 2 },
          { label: "2026-09-05", value: 3 },
        ]}
        ariaLabel="credits history"
        animations={false}
      />,
    );

    expect(container.querySelectorAll(".chart__point")).toHaveLength(4);
    expect(container.querySelectorAll(".chart__line")).toHaveLength(2);
    expect(container).toHaveTextContent("2026-09-02: 0.00");
    expect(container).not.toHaveTextContent("2026-09-03: 0.00");
  });

  it("keeps full endpoint dates in the axis", () => {
    const { container } = render(
      <LineChart
        data={[
          { label: "2026-09-01", value: 1 },
          { label: "2026-09-30", value: 2 },
        ]}
        ariaLabel="credits history"
        animations={false}
      />,
    );

    const labels = container.querySelectorAll(".chart__axis > span");
    expect(labels).toHaveLength(2);
    expect(labels[0]).toHaveTextContent("2026-09-01");
    expect(labels[1]).toHaveTextContent("2026-09-30");
    expect((labels[0] as HTMLElement).style.left).toBe("36px");
    expect((labels[1] as HTMLElement).style.left).toBe("244px");
    expect(container.querySelector(".chart__axis-max")).toBeNull();
    expect(labels[0]).toHaveClass("chart__axis-start");
    expect(labels[1]).toHaveClass("chart__axis-end");
    expect((labels[0] as HTMLElement).style.transform).toBe("");
    expect((labels[1] as HTMLElement).style.transform).toBe("");
  });
});
