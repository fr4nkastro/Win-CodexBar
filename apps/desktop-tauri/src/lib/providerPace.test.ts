import { describe, expect, it } from "vitest";
import { providerAllowsPace } from "./providerPace";

describe("providerAllowsPace", () => {
  it("rejects device-local OpenCode Go estimates", () => {
    expect(providerAllowsPace("opencodego", " LOCAL ESTIMATE ")).toBe(false);
  });

  it("keeps pace enabled for authoritative and unrelated sources", () => {
    expect(providerAllowsPace("opencodego", "api")).toBe(true);
    expect(providerAllowsPace("opencodego", "web")).toBe(true);
    expect(providerAllowsPace("claude", "local estimate")).toBe(true);
    expect(providerAllowsPace("opencodego", null)).toBe(true);
  });
});
