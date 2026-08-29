import { readFileSync } from "node:fs";
import { act, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type {
  ClaudeAccount,
  ClaudeAccountsStateBridge,
  ClaudeAccountUsageSnapshot,
} from "../../../../../types/bridge";

const tauriMocks = vi.hoisted(() => ({
  getClaudeAccountsState: vi.fn(),
  claudeAccountAdd: vi.fn(),
  claudeAccountFetch: vi.fn(),
  claudeAccountRemove: vi.fn(),
  claudeAccountSwitch: vi.fn(),
  refreshProviders: vi.fn().mockResolvedValue(undefined),
  getSettingsSnapshot: vi.fn(),
}));

const eventMocks = vi.hoisted(() => ({
  listen: vi.fn(() => Promise.resolve(() => {})),
}));

vi.mock("../../../../../lib/tauri", () => tauriMocks);
vi.mock("@tauri-apps/api/event", () => eventMocks);

import { ClaudeAccountsSection } from "./ClaudeAccountsSection";

const t = (key: string) => key;

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

function snapshot(
  usedPercent: number,
  plan = "max",
): ClaudeAccountUsageSnapshot {
  return {
    email: "user@example.com",
    orgId: null,
    plan,
    primaryWindow: { usedPercent, resetAt: null, limitWindowSeconds: 3600 },
    secondaryWindow: null,
    updatedAt: "2024-01-01T00:00:00Z",
  };
}

describe("ClaudeAccountsSection", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    tauriMocks.getSettingsSnapshot.mockResolvedValue({
      claudeAllowManagingClaudeCodeAccounts: true,
    });
  });

  it("stays hidden while the consent flag is off", async () => {
    tauriMocks.getSettingsSnapshot.mockResolvedValue({
      claudeAllowManagingClaudeCodeAccounts: false,
    });
    tauriMocks.getClaudeAccountsState.mockResolvedValue({
      accounts: [account("1")],
      snapshots: {},
    } as ClaudeAccountsStateBridge);
    const { container } = render(<ClaudeAccountsSection t={t} />);
    await waitFor(() => {
      expect(tauriMocks.getSettingsSnapshot).toHaveBeenCalledTimes(1);
    });
    expect(container.querySelector(".claude-accounts")).toBeNull();
    expect(tauriMocks.getClaudeAccountsState).not.toHaveBeenCalled();
  });

  it("renders nothing before the store loads, then lists accounts", async () => {
    tauriMocks.getClaudeAccountsState.mockResolvedValue({
      accounts: [account("1"), account("2", { source: "ambient" })],
      snapshots: {},
    } as ClaudeAccountsStateBridge);
    const { container } = render(<ClaudeAccountsSection t={t} />);
    expect(container.querySelector(".claude-accounts")).toBeNull();

    await waitFor(() => {
      expect(screen.getByText("user-1@example.com")).toBeDefined();
    });
    expect(screen.getByText("user-2@example.com")).toBeDefined();
    expect(screen.getByText("ClaudeAccountsSourceManaged")).toBeDefined();
    expect(screen.getByText("ClaudeAccountsSourceAmbient")).toBeDefined();
  });

  it("shows the usage pill from a snapshot", async () => {
    tauriMocks.getClaudeAccountsState.mockResolvedValue({
      accounts: [account("1")],
      snapshots: {
        "1": snapshot(38),
      },
    } as ClaudeAccountsStateBridge);
    render(<ClaudeAccountsSection t={t} />);
    await waitFor(() => {
      expect(screen.getByText("max · 38%")).toBeDefined();
    });
  });

  it("adds an account and reloads", async () => {
    tauriMocks.getClaudeAccountsState.mockResolvedValueOnce({
      accounts: [],
      snapshots: {},
    } as ClaudeAccountsStateBridge);
    tauriMocks.getClaudeAccountsState.mockResolvedValueOnce({
      accounts: [account("1")],
      snapshots: {},
    } as ClaudeAccountsStateBridge);
    render(<ClaudeAccountsSection t={t} />);
    await waitFor(() => {
      expect(screen.getByText("ClaudeAccountsAddButton")).toBeDefined();
    });

    tauriMocks.claudeAccountAdd.mockResolvedValue(account("1"));
    await act(async () => {
      screen.getByText("ClaudeAccountsAddButton").click();
    });
    await waitFor(() => {
      expect(screen.getByText("user-1@example.com")).toBeDefined();
    });
    expect(tauriMocks.claudeAccountAdd).toHaveBeenCalledTimes(1);
  });

  it("switches an account, shows success, and triggers a provider refresh", async () => {
    tauriMocks.getClaudeAccountsState.mockResolvedValue({
      accounts: [account("1")],
      snapshots: {},
    } as ClaudeAccountsStateBridge);
    tauriMocks.claudeAccountSwitch.mockResolvedValue({
      materializedAccount: null,
      backupPath: null,
      ambientAccount: null,
    });
    render(<ClaudeAccountsSection t={t} />);
    await waitFor(() => {
      expect(screen.getByText("ClaudeAccountsSwitchButton")).toBeDefined();
    });

    await act(async () => {
      screen.getByText("ClaudeAccountsSwitchButton").click();
    });
    await waitFor(() => {
      expect(screen.getByText("ClaudeSwitchSuccess")).toBeDefined();
    });
    expect(tauriMocks.refreshProviders).toHaveBeenCalledTimes(1);
  });

  it("keeps the switch success state when the provider refresh rejects", async () => {
    tauriMocks.getClaudeAccountsState.mockResolvedValue({
      accounts: [account("1")],
      snapshots: {},
    } as ClaudeAccountsStateBridge);
    tauriMocks.claudeAccountSwitch.mockResolvedValue({
      materializedAccount: null,
      backupPath: null,
      ambientAccount: null,
    });
    tauriMocks.refreshProviders.mockRejectedValueOnce(new Error("refresh boom"));
    render(<ClaudeAccountsSection t={t} />);
    await waitFor(() => {
      expect(screen.getByText("ClaudeAccountsSwitchButton")).toBeDefined();
    });

    await act(async () => {
      screen.getByText("ClaudeAccountsSwitchButton").click();
    });
    await waitFor(() => {
      expect(screen.getByText("ClaudeSwitchSuccess")).toBeDefined();
    });
    expect(tauriMocks.refreshProviders).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("marks only the activeAccountId row active and disables its switch", async () => {
    tauriMocks.getClaudeAccountsState.mockResolvedValue({
      accounts: [account("1"), account("2")],
      snapshots: {},
      activeAccountId: "2",
    } as ClaudeAccountsStateBridge);
    render(<ClaudeAccountsSection t={t} />);
    await waitFor(() => {
      expect(screen.getByText("user-1@example.com")).toBeDefined();
    });

    expect(screen.getAllByText("ClaudeAccountsActive")).toHaveLength(1);

    const switches = screen.getAllByText("ClaudeAccountsSwitchButton");
    // account("1") renders first, account("2") second.
    expect((switches[0] as HTMLButtonElement).disabled).toBe(false);
    expect((switches[1] as HTMLButtonElement).disabled).toBe(true);
  });

  it("renders no active row when activeAccountId is absent", async () => {
    tauriMocks.getClaudeAccountsState.mockResolvedValue({
      accounts: [account("1"), account("2")],
      snapshots: {},
    } as ClaudeAccountsStateBridge);
    render(<ClaudeAccountsSection t={t} />);
    await waitFor(() => {
      expect(screen.getByText("user-1@example.com")).toBeDefined();
    });

    expect(screen.queryByText("ClaudeAccountsActive")).toBeNull();
    for (const button of screen.getAllByText("ClaudeAccountsSwitchButton")) {
      expect((button as HTMLButtonElement).disabled).toBe(false);
    }
  });

  it("removes an account and reloads", async () => {
    tauriMocks.getClaudeAccountsState.mockResolvedValueOnce({
      accounts: [account("1")],
      snapshots: {},
    } as ClaudeAccountsStateBridge);
    tauriMocks.getClaudeAccountsState.mockResolvedValueOnce({
      accounts: [],
      snapshots: {},
    } as ClaudeAccountsStateBridge);
    render(<ClaudeAccountsSection t={t} />);
    await waitFor(() => {
      expect(screen.getByText("ClaudeAccountsRemoveButton")).toBeDefined();
    });

    tauriMocks.claudeAccountRemove.mockResolvedValue(undefined);
    await act(async () => {
      screen.getByText("ClaudeAccountsRemoveButton").click();
    });
    await waitFor(() => {
      expect(screen.getByText("ClaudeAccountsEmpty")).toBeDefined();
    });
    expect(tauriMocks.claudeAccountRemove).toHaveBeenCalledWith("1");
  });
});

// Layout containment cannot be asserted via jsdom (vitest runs with
// `css: false`, so styles.css is never applied and computed styles are
// empty). Assert the stylesheet rules directly instead — mirrors the Codex
// accounts section's own containment test. import.meta.dirname (not .url)
// survives vitest's jsdom transform as the real on-disk directory.
if (!import.meta.dirname) {
  throw new Error("import.meta.dirname unavailable to vitest runner");
}
const stylesSource = readFileSync(
  `${import.meta.dirname}/../../../../../styles.css`,
  "utf8",
);

function ruleBlock(source: string, selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const match = source.match(new RegExp(`${escaped}\\s*\\{([^}]*)\\}`));
  expect(match).not.toBeNull();
  return match![1];
}

describe("ClaudeAccountsSection containment styles", () => {
  it("ellipsizes the info column and pins the actions row inside the card", () => {
    const info = ruleBlock(
      stylesSource,
      ".claude-accounts-card .credential-card__info",
    );
    expect(info).toContain("min-width: 0");
    expect(info).toContain("overflow: hidden");
    expect(info).toContain("text-overflow: ellipsis");
    expect(info).toContain("white-space: nowrap");

    const title = ruleBlock(
      stylesSource,
      ".claude-accounts-card .credential-card__info strong",
    );
    expect(title).toContain("max-width: 100%");
    expect(title).toContain("overflow: hidden");
    expect(title).toContain("text-overflow: ellipsis");
    expect(title).toContain("white-space: nowrap");

    const actions = ruleBlock(
      stylesSource,
      ".claude-accounts-card .credential-card__actions",
    );
    expect(actions).toContain("flex-shrink: 0");
    expect(actions).toContain("nowrap");
  });
});
