import { render } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { UsageBreakdownChart } from "./UsageBreakdownChart";

describe("UsageBreakdownChart", () => {
  it("keeps full calendar dates for stacked-bar rows", () => {
    const { container } = render(
      <UsageBreakdownChart
        data={[
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
        title="Usage"
        ariaLabel="usage breakdown"
        animations={false}
        emptyMessage="No history"
      />,
    );

    const labels = container.querySelectorAll(".chart__row-label");
    expect(labels).toHaveLength(2);
    expect(labels[0]).toHaveTextContent("2026-09-01");
    expect(labels[1]).toHaveTextContent("2026-09-30");
  });
});
