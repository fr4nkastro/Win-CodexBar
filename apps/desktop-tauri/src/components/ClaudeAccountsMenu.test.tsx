import { act, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type {
  ClaudeAccount,
  ClaudeAccountsStateBridge,
  ClaudeAccountUsageSnapshot,
} from "../types/bridge";
import { buildBundle } from "../test/localeHarness";
import { LocaleProvider } from "../i18n/LocaleProvider";

const tauriMocks = vi.hoisted(() => ({
  getClaudeAccountsState: vi.fn(),
  claudeAccountSwitch: vi.fn(),
  refreshProviders: vi.fn().mockResolvedValue(undefined),
  getLocaleStrings: vi.fn(),
}));

const eventMocks = vi.hoisted(() => ({
  listen: vi.fn(() => Promise.resolve(() => {})),
}));

vi.mock("../lib/tauri", () => tauriMocks);
vi.mock("@tauri-apps/api/event", () => eventMocks);

import ClaudeAccountsMenu from "./ClaudeAccountsMenu";

function account(
  id: string,
  extra: Partial<ClaudeAccount> = {},
): ClaudeAccount {
  return {
    id,
    nickname: null,
    emailHint: `user-${id}@example.com`,
    orgId: null,
    orgName: null,
    subscriptionType: null,
    claudeConfigDir: `C:/fake/${id}`,
    source: "managedByApp",
    createdAt: "2024-01-01T00:00:00Z",
    updatedAt: "2024-01-01T00:00:00Z",
    lastAuthenticatedAt: null,
    ...extra,
  };
}

function snapshot(usedPercent: number): ClaudeAccountUsageSnapshot {
  return {
    email: "user@example.com",
    orgId: null,
    plan: "max",
    primaryWindow: { usedPercent, resetAt: null, limitWindowSeconds: 3600 },
    secondaryWindow: null,
    updatedAt: "2024-01-01T00:00:00Z",
  };
}

// Wrap the component so the `t` from useLocale is a stable identity that just
// returns the key (the component uses `t(key)` for locale strings and a badge
// label; returning the key is enough to assert rendering).
function renderMenu(hideEmail: boolean, state: ClaudeAccountsStateBridge) {
  tauriMocks.getClaudeAccountsState.mockResolvedValue(state);
  tauriMocks.getLocaleStrings.mockResolvedValue(buildBundle({}));
  return render(
    <LocaleProvider>
      <ClaudeAccountsMenu hideEmail={hideEmail} />
    </LocaleProvider>,
  );
}

describe("ClaudeAccountsMenu", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("renders nothing for a single-account setup (single-account fallback)", async () => {
    const { container } = renderMenu(false, {
      accounts: [account("1", { source: "ambient" })],
      snapshots: {},
    });
    await waitFor(() => {
      expect(container.querySelector(".claude-menu-accounts")).toBeNull();
    });
  });

  it("marks the activeAccountId row active even when every account is managed", async () => {
    const { container } = renderMenu(false, {
      accounts: [account("1"), account("2")],
      snapshots: { "1": snapshot(30), "2": snapshot(70) },
      activeAccountId: "1",
    });
    await screen.findByText("user-1@example.com");
    expect(screen.getByText("user-2@example.com")).toBeDefined();

    const rows = container.querySelectorAll(".claude-menu-accounts__row");
    expect(rows.length).toBe(2);
    // The active row is keyed off activeAccountId, not source === "ambient".
    expect(
      rows[0].className.includes("claude-menu-accounts__row--active"),
    ).toBe(true);
    expect(
      rows[1].className.includes("claude-menu-accounts__row--active"),
    ).toBe(false);
    expect(
      (
        rows[0].querySelector(
          ".claude-menu-accounts__switch",
        ) as HTMLButtonElement
      ).disabled,
    ).toBe(true);
    expect(
      (
        rows[1].querySelector(
          ".claude-menu-accounts__switch",
        ) as HTMLButtonElement
      ).disabled,
    ).toBe(false);
    expect(screen.getAllByText("ClaudeAccountsActive")).toHaveLength(1);

    // Usage bar widths map to the snapshot percentages.
    const fills = container.querySelectorAll(
      ".claude-menu-accounts__bar-fill",
    );
    expect((fills[0] as HTMLElement).style.width).toBe("30%");
    expect((fills[1] as HTMLElement).style.width).toBe("70%");
  });

  it("renders a usage bar from a weekly-only snapshot (primaryWindow: null)", async () => {
    const weeklyOnly: ClaudeAccountUsageSnapshot = {
      email: "weekly@example.com",
      orgId: null,
      plan: "pro",
      primaryWindow: null,
      secondaryWindow: {
        usedPercent: 42,
        resetAt: null,
        limitWindowSeconds: 604800,
      },
      updatedAt: "2024-01-01T00:00:00Z",
    };
    const { container } = renderMenu(false, {
      accounts: [account("1", { source: "ambient" }), account("2")],
      snapshots: { "1": weeklyOnly },
    });
    await screen.findByText("user-1@example.com");

    const fills = container.querySelectorAll(
      ".claude-menu-accounts__bar-fill",
    );
    expect(fills.length).toBe(1);
    expect((fills[0] as HTMLElement).style.width).toBe("42%");
  });

  it("switches an account and kicks a provider refresh", async () => {
    renderMenu(false, {
      accounts: [account("1"), account("2")],
      snapshots: {},
      activeAccountId: "1",
    });
    await screen.findByText("user-1@example.com");

    tauriMocks.claudeAccountSwitch.mockResolvedValue({});
    tauriMocks.getClaudeAccountsState.mockResolvedValue({
      accounts: [account("1"), account("2")],
      snapshots: {},
      activeAccountId: "1",
    });
    const switchButtons = screen.getAllByText("ClaudeAccountsSwitchButton");
    const activeSwitch = switchButtons.find(
      (b) => !(b as HTMLButtonElement).disabled,
    );
    expect(activeSwitch).toBeDefined();
    await act(async () => {
      activeSwitch!.click();
    });
    expect(tauriMocks.claudeAccountSwitch).toHaveBeenCalledWith("2");
    expect(tauriMocks.refreshProviders).toHaveBeenCalledTimes(1);
  });
});
