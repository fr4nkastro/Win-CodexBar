import { act, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LocaleProvider } from "../i18n/LocaleProvider";
import { buildBundle } from "../test/localeHarness";
import {
  normalizeResetDescription,
  useFormattedResetTime,
  type ResetTimeFormatMode,
} from "./useFormattedResetTime";
import * as tauri from "../lib/tauri";

vi.mock("../lib/tauri", () => ({
  getLocaleStrings: vi.fn(),
  setUiLanguage: vi.fn(),
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn().mockResolvedValue(() => {}),
}));

function Probe({
  resetsAt,
  fallback,
  relative,
  mode,
}: {
  resetsAt: string | null;
  fallback: string | null;
  relative: boolean;
  mode?: ResetTimeFormatMode;
}) {
  const text = useFormattedResetTime(resetsAt, fallback, relative, mode);
  return <span data-testid="reset">{text ?? "null"}</span>;
}

async function mountWithLocale(ui: React.ReactNode) {
  (tauri.getLocaleStrings as ReturnType<typeof vi.fn>).mockResolvedValue(
    buildBundle({
      MetricResetsIn: "Resets in",
      ResetsInHoursMinutes: "Resets in {}h {}m",
      ResetsInMinutes: "Resets in {}m",
      ResetsInDaysHours: "Resets in {}d {}h",
      TrayResetsDueNow: "Resetting",
      NextExpiresInHoursMinutes: "Next expires in {}h {}m",
      NextExpiresInMinutes: "Next expires in {}m",
      NextExpiresInDaysHours: "Next expires in {}d {}h",
      NextExpiresDueNow: "Expires now",
    }),
  );
  const rendered = render(<LocaleProvider>{ui}</LocaleProvider>);
  await act(async () => {});
  return rendered;
}

describe("useFormattedResetTime", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2024-06-01T00:00:00Z"));
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it.each([
    ["Reset", "Resets"],
    ["Resets", "Resets"],
    ["Reset Jul 10 at 2:59am (Europe/Prague)", "Resets Jul 10 at 2:59am (Europe/Prague)"],
    ["Reset in 11m", "Resets in 11m"],
    ["Resets in 11m", "Resets in 11m"],
    ["Reset at 23:30 (UTC)", "Resets at 23:30 (UTC)"],
    ["  rEsEt In 2h 5m \n", "Resets in 2h 5m"],
    ["Reset demain à 23:30", "Resets demain à 23:30"],
    ["at 23:30 (UTC)", "Resets at 23:30 (UTC)"],
    ["Resetting soon", "Resets Resetting soon"],
    ["   \n\t", null],
  ] as const)("normalizes reset description %j", (description, expected) => {
    expect(normalizeResetDescription(description)).toBe(expected);
  });

  it("returns a complete localized countdown in relative mode", async () => {
    const target = new Date("2024-06-01T03:42:00Z").toISOString();
    await mountWithLocale(
      <Probe resetsAt={target} fallback="later" relative={true} />,
    );
    expect(screen.getByTestId("reset")).toHaveTextContent("Resets in 3h 42m");
  });

  it("omits zero hours for sub-hour resets", async () => {
    const target = new Date("2024-06-01T00:40:00Z").toISOString();
    await mountWithLocale(
      <Probe resetsAt={target} fallback="later" relative={true} />,
    );
    expect(screen.getByTestId("reset")).toHaveTextContent("Resets in 40m");
  });

  it("normalizes a fallback reset description in relative mode", async () => {
    await mountWithLocale(
      <Probe resetsAt={null} fallback="Reset in 3h" relative={true} />,
    );
    expect(screen.getByTestId("reset")).toHaveTextContent("Resets in 3h");
  });

  it("gives a parsed reset timestamp precedence over fallback wording", async () => {
    const target = new Date("2024-06-01T03:42:00Z").toISOString();
    await mountWithLocale(
      <Probe resetsAt={target} fallback="Reset in 99h" relative={true} />,
    );
    expect(screen.getByTestId("reset")).toHaveTextContent("Resets in 3h 42m");
  });

  it("returns an absolute local time without the reset label", async () => {
    const target = new Date("2024-06-01T03:42:00Z").toISOString();
    await mountWithLocale(
      <Probe resetsAt={target} fallback="later" relative={false} />,
    );
    expect(screen.getByTestId("reset")).not.toHaveTextContent("Resets in");
  });

  it('uses Next expires wording when mode is "expires"', async () => {
    const target = new Date("2024-06-01T03:42:00Z").toISOString();
    await mountWithLocale(
      <Probe resetsAt={target} fallback="later" relative={true} mode="expires" />,
    );
    expect(screen.getByTestId("reset")).toHaveTextContent("Next expires in 3h 42m");
  });
});
