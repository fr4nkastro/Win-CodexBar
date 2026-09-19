import { act, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type {
  CodexAccount,
  CodexAccountsStateBridge,
  CodexAccountUsageSnapshot,
} from "../types/bridge";
import { buildBundle } from "../test/localeHarness";
import { LocaleProvider } from "../i18n/LocaleProvider";

const tauriMocks = vi.hoisted(() => ({
  getCodexAccountsState: vi.fn(),
  codexAccountSwitch: vi.fn(),
  refreshProviders: vi.fn().mockResolvedValue(undefined),
  getLocaleStrings: vi.fn(),
}));

const eventMocks = vi.hoisted(() => ({
  listen: vi.fn(() => Promise.resolve(() => {})),
}));

vi.mock("../lib/tauri", () => tauriMocks);
vi.mock("@tauri-apps/api/event", () => eventMocks);

import CodexAccountsMenu from "./CodexAccountsMenu";

function account(id: string, extra: Partial<CodexAccount> = {}): CodexAccount {
  return {
    id,
    nickname: null,
    emailHint: `user-${id}@example.com`,
    authSubject: null,
    providerAccountId: null,
    codexHomePath: `C:/fake/${id}`,
    source: "managedByApp",
    createdAt: "2024-01-01T00:00:00Z",
    updatedAt: "2024-01-01T00:00:00Z",
    lastAuthenticatedAt: null,
    ...extra,
  };
}

function snapshot(
  usedPercent: number,
  resetAt: string | null = null,
): CodexAccountUsageSnapshot {
  return {
    email: "user@example.com",
    providerAccountId: null,
    plan: "free",
    allowed: true,
    limitReached: false,
    primaryWindow: { usedPercent, resetAt, limitWindowSeconds: 18_000 },
    secondaryWindow: null,
    credits: null,
    updatedAt: "2024-01-01T00:00:00Z",
  };
}

// Wrap the component so the `t` from useLocale is a stable identity that just
// returns the key (the component uses `t(key)` for locale strings and a badge
// label; returning the key is enough to assert rendering).
function renderMenu(
  hideEmail: boolean,
  state: CodexAccountsStateBridge,
  resetTimeRelative = true,
) {
  tauriMocks.getCodexAccountsState.mockResolvedValue(state);
  tauriMocks.getLocaleStrings.mockResolvedValue(buildBundle({}));
  return render(
    <LocaleProvider>
      <CodexAccountsMenu
        hideEmail={hideEmail}
        resetTimeRelative={resetTimeRelative}
      />
    </LocaleProvider>,
  );
}

describe("CodexAccountsMenu", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("renders nothing for a single-account setup (single-account fallback)", async () => {
    const { container } = renderMenu(false, {
      accounts: [account("1", { source: "ambient" })],
      accountOrdinals: { "1": 1 },
      snapshots: {},
    });
    await waitFor(() => {
      expect(
        container.querySelector(".codex-menu-accounts"),
      ).toBeNull();
    });
  });

  it("marks the activeAccountId row active even when every account is managed", async () => {
    const { container } = renderMenu(false, {
      accounts: [account("1"), account("2")],
      accountOrdinals: { "1": 1, "2": 2 },
      snapshots: { "1": snapshot(30), "2": snapshot(70) },
      activeAccountId: "1",
    });
    await screen.findByText("user-1@example.com");
    expect(screen.getByText("user-2@example.com")).toBeDefined();

    const rows = container.querySelectorAll(".codex-menu-accounts__row");
    expect(rows.length).toBe(2);
    // The active row is keyed off activeAccountId, not source === "ambient".
    expect(
      rows[0].className.includes("codex-menu-accounts__row--active"),
    ).toBe(true);
    expect(
      rows[1].className.includes("codex-menu-accounts__row--active"),
    ).toBe(false);
    expect(
      (rows[0].querySelector(".codex-menu-accounts__switch") as HTMLButtonElement)
        .disabled,
    ).toBe(true);
    expect(
      (rows[1].querySelector(".codex-menu-accounts__switch") as HTMLButtonElement)
        .disabled,
    ).toBe(false);
    expect(screen.getAllByText("CodexAccountsActive")).toHaveLength(1);

    // Usage bar widths map to the snapshot percentages.
    const fills = container.querySelectorAll(".codex-menu-accounts__bar-fill");
    expect((fills[0] as HTMLElement).style.width).toBe("30%");
    expect((fills[1] as HTMLElement).style.width).toBe("70%");
  });

  it("renders a usage bar from a weekly-only snapshot (primaryWindow: null)", async () => {
    const weeklyOnly: CodexAccountUsageSnapshot = {
      email: "weekly@example.com",
      providerAccountId: null,
      plan: "pro",
      allowed: true,
      limitReached: false,
      primaryWindow: null,
      secondaryWindow: {
        usedPercent: 42,
        resetAt: null,
        limitWindowSeconds: 604800,
      },
      credits: null,
      updatedAt: "2024-01-01T00:00:00Z",
    };
    const { container } = renderMenu(false, {
      accounts: [account("1", { source: "ambient" }), account("2")],
      accountOrdinals: { "1": 1, "2": 2 },
      snapshots: { "1": weeklyOnly },
    });
    await screen.findByText("user-1@example.com");

    const fills = container.querySelectorAll(
      ".codex-menu-accounts__bar-fill",
    );
    expect(fills.length).toBe(1);
    expect((fills[0] as HTMLElement).style.width).toBe("42%");
  });

  it("shows the five-hour usage and local reset time for each account", async () => {
    const resetAt = "2030-01-02T03:04:00Z";
    const expectedReset = new Intl.DateTimeFormat(undefined, {
      month: "short",
      day: "numeric",
      hour: "numeric",
      minute: "2-digit",
    }).format(new Date(resetAt));

    renderMenu(
      false,
      {
        accounts: [account("1", { source: "ambient" }), account("2")],
        accountOrdinals: { "1": 1, "2": 2 },
        snapshots: { "1": snapshot(30, resetAt), "2": snapshot(70, resetAt) },
      },
      false,
    );

    await screen.findByText("user-1@example.com");
    expect(screen.getAllByText("5h")).toHaveLength(2);
    expect(screen.getByText("30% PanelUsedSuffix")).toBeDefined();
    expect(screen.getAllByText(`MetricResetsIn ${expectedReset}`)).toHaveLength(2);
  });

  it("switches an account and kicks a provider refresh", async () => {
    renderMenu(false, {
      accounts: [account("1", { source: "ambient" }), account("2")],
      accountOrdinals: { "1": 1, "2": 2 },
      snapshots: {},
      activeAccountId: "1",
    });
    await screen.findByText("user-1@example.com");

    tauriMocks.codexAccountSwitch.mockResolvedValue({});
    tauriMocks.getCodexAccountsState.mockResolvedValue({
      accounts: [account("1", { source: "ambient" }), account("2")],
      accountOrdinals: { "1": 1, "2": 2 },
      snapshots: {},
      activeAccountId: "1",
    });
    const switchButtons = screen.getAllByText("CodexAccountsSwitchButton");
    const activeSwitch = switchButtons.find((b) => !(b as HTMLButtonElement).disabled);
    expect(activeSwitch).toBeDefined();
    await act(async () => {
      activeSwitch!.click();
    });
    expect(tauriMocks.codexAccountSwitch).toHaveBeenCalledWith("2");
    expect(tauriMocks.refreshProviders).toHaveBeenCalledTimes(1);
  });
  it("uses opaque ordinal labels and matching tooltips while hideEmail is on", async () => {
    const { container: hidden } = renderMenu(true, {
      accounts: [account("1", { source: "ambient" }), account("2")],
      accountOrdinals: { "1": 1, "2": 2 },
      snapshots: {},
    });
    await waitFor(() => {
      expect(
        hidden.querySelectorAll(".codex-menu-accounts__email").length,
      ).toBe(2);
    });
    const hiddenEmail = hidden.querySelectorAll(
      ".codex-menu-accounts__email",
    )[1] as HTMLElement;
    expect(hiddenEmail.getAttribute("title")).toBe(hiddenEmail.textContent);
    expect(hiddenEmail.textContent).toBe("Account 2");
    expect(hiddenEmail.textContent).not.toContain("@");
    expect(hiddenEmail.textContent).not.toContain("example.com");

    const { container: visible } = renderMenu(false, {
      accounts: [account("1", { source: "ambient" }), account("2")],
      accountOrdinals: { "1": 1, "2": 2 },
      snapshots: {},
    });
    await waitFor(() => {
      expect(
        visible.querySelectorAll(".codex-menu-accounts__email").length,
      ).toBe(2);
    });
    const rawEmail = visible.querySelectorAll(
      ".codex-menu-accounts__email",
    )[1] as HTMLElement;
    expect(rawEmail.getAttribute("title")).toBe("user-2@example.com");
  });

  it("uses canonical opaque ordinals supplied by the account bridge", async () => {
    const first = account("uuid-b", {
      emailHint: "alice@example.com",
      nickname: "team@example.com",
    });
    const second = account("uuid-a", {
      emailHint: "bob@example.com",
      nickname: "Private workspace",
    });

    const { container } = renderMenu(true, {
      accounts: [first, second],
      accountOrdinals: { "uuid-b": 2, "uuid-a": 1 },
      snapshots: {},
    });
    await screen.findByText("Account 2");
    const labels = container.querySelectorAll(".codex-menu-accounts__email");
    expect(labels[0].textContent).toBe("Account 2");
    expect(labels[1].textContent).toBe("Account 1");
    expect(labels[0].textContent).not.toContain("@");
    expect(labels[1].textContent).not.toContain("example.com");
  });
});

