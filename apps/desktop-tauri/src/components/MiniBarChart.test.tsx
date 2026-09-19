import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { SimpleBarChart, StackedBarChart } from "./MiniBarChart";

const t = (key: string) => key;

describe("MiniBarChart history axes", () => {
  it("keeps full endpoint dates for cost history", () => {
    const { container } = render(
      <SimpleBarChart
        points={[
          { date: "2026-09-01", value: null },
          { date: "2026-09-30", value: 2 },
        ]}
        t={t}
      />,
    );

    const labels = container.querySelectorAll(".mini-chart__axis > span");
    expect(labels).toHaveLength(2);
    expect(labels[0]).toHaveTextContent("2026-09-01");
    expect(labels[1]).toHaveTextContent("2026-09-30");
    expect((labels[0] as HTMLElement).style.left).toBe("87.5px");
    expect((labels[1] as HTMLElement).style.left).toBe("192.5px");
    expect(container.querySelector(".mini-chart__axis-max")).toBeNull();
    expect(labels[0]).toHaveClass("mini-chart__axis-start");
    expect(labels[1]).toHaveClass("mini-chart__axis-end");
    expect((labels[0] as HTMLElement).style.transform).toBe("");
    expect((labels[1] as HTMLElement).style.transform).toBe("");
  });

  it("keeps full endpoint dates for usage breakdown history", () => {
    const { container } = render(
      <StackedBarChart
        points={[
          {
            day: "2026-09-01",
            services: [{ service: "cli", creditsUsed: 1 }],
            totalCreditsUsed: 1,
          },
          {
            day: "2026-09-30",
            services: [{ service: "api", creditsUsed: 2 }],
            totalCreditsUsed: 2,
          },
        ]}
        t={t}
      />,
    );

    const labels = container.querySelectorAll(".mini-chart__axis > span");
    expect(labels).toHaveLength(2);
    expect(labels[0]).toHaveTextContent("2026-09-01");
    expect(labels[1]).toHaveTextContent("2026-09-30");
    expect((labels[0] as HTMLElement).style.left).toBe("87.5px");
    expect((labels[1] as HTMLElement).style.left).toBe("192.5px");
    expect(container.querySelector(".mini-chart__axis-max")).toBeNull();
    expect(labels[0]).toHaveClass("mini-chart__axis-start");
    expect(labels[1]).toHaveClass("mini-chart__axis-end");
    expect((labels[0] as HTMLElement).style.transform).toBe("");
    expect((labels[1] as HTMLElement).style.transform).toBe("");
  });
});
