import { useCallback, useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import type {
  ClaudeAccount,
  ClaudeAccountsStateBridge,
  ClaudeAccountUsageSnapshot,
} from "../types/bridge";
import { useLocale } from "../hooks/useLocale";
import { maskEmail } from "./MenuCard";
import {
  claudeAccountCancelLogin,
  claudeAccountSwitch,
  getClaudeAccountsState,
  refreshProviders,
} from "../lib/tauri";

/**
 * Multi-account lane surface for the Claude tray menu card
 * (claude-multi-account, 1:1 port of `CodexAccountsMenu.tsx`). Renders only
 * when more than one Claude account exists, so the common single-account
 * menu stays unchanged (single-account fallback).
 *
 * Shows every account (ambient + managed) with a compact usage bar and a
 * Switch action. Switching updates the ambient identity and triggers a
 * provider refresh so the tray icon/menu reflect the now-active account.
 * While an Add-account sign-in (started from Settings or the native tray
 * menu) is running, a Cancel sign-in action is shown even for one account.
 */
export default function ClaudeAccountsMenu({
  hideEmail,
  onLayoutChange,
}: {
  hideEmail: boolean;
  onLayoutChange?: () => void;
}) {
  const { t } = useLocale();
  const [accounts, setAccounts] = useState<ClaudeAccount[]>([]);
  const [snapshots, setSnapshots] = useState<
    Record<string, ClaudeAccountUsageSnapshot>
  >({});
  const [activeAccountId, setActiveAccountId] = useState<string | null>(null);
  const [loginInProgress, setLoginInProgress] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const next: ClaudeAccountsStateBridge = await getClaudeAccountsState();
      setAccounts(next.accounts);
      setSnapshots(next.snapshots);
      setActiveAccountId(next.activeAccountId ?? null);
      setLoginInProgress(next.loginInProgress ?? false);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  useEffect(() => {
    onLayoutChange?.();
  }, [accounts.length, error, loginInProgress, onLayoutChange]);

  useEffect(() => {
    let cancelled = false;
    const unlistenPromise = listen("claude-accounts-updated", () => {
      if (!cancelled) void load();
    });
    return () => {
      cancelled = true;
      void unlistenPromise.then((fn) => fn());
    };
  }, [load]);

  const handleSwitch = async (id: string) => {
    setBusy(true);
    setError(null);
    try {
      await claudeAccountSwitch(id);
      await load();
      // Make the tray icon/menu reflect the newly active ambient identity.
      void refreshProviders().catch(() => {});
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  const handleCancelLogin = async () => {
    setError(null);
    try {
      await claudeAccountCancelLogin();
      await load();
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  const cancelRow = loginInProgress ? (
    <div className="claude-menu-accounts__signing-in" role="status">
      <span>{t("ClaudeAccountsSigningIn")}</span>
      <button
        type="button"
        className="claude-menu-accounts__switch"
        onClick={() => void handleCancelLogin()}
      >
        {t("ClaudeAccountsCancelLogin")}
      </button>
    </div>
  ) : null;

  if (accounts.length <= 1) {
    return cancelRow;
  }

  return (
    <>
      {cancelRow}
      <details className="claude-menu-accounts" onToggle={onLayoutChange}>
      <summary className="claude-menu-accounts__summary">
        <span className="claude-menu-accounts__title">
          {t("ClaudeAccountsTitle")}
        </span>
        <span className="claude-menu-accounts__count">{accounts.length}</span>
      </summary>
      {error && (
        <div className="claude-menu-accounts__error" role="alert">
          {error}
        </div>
      )}
      <ul className="claude-menu-accounts__list">
        {accounts.map((account) => {
          const snapshot = snapshots[account.id];
          // Prefer the primary (session) window, but accounts whose backend
          // only returns a weekly window have primaryWindow: null — fall back
          // to the secondary window (Claude's snapshot carries no tertiary or
          // extra rate windows) so the usage bar still renders.
          const usageWindow =
            snapshot?.primaryWindow ?? snapshot?.secondaryWindow ?? null;
          const pct = usageWindow ? Math.round(usageWindow.usedPercent) : null;
          const label =
            account.nickname ??
            account.emailHint ??
            account.orgName ??
            shrink(account.id);
          const shown = hideEmail ? maskEmail(label) : label;
          const isAmbient = account.source === "ambient";
          const isActive = account.id === activeAccountId;
          return (
            <li key={account.id}>
              <div
                className={`claude-menu-accounts__row${isActive ? " claude-menu-accounts__row--active" : ""}`}
              >
                <div className="claude-menu-accounts__meta">
                  <span className="claude-menu-accounts__email" title={label}>
                    {shown}
                    {isActive && (
                      <span className="claude-menu-accounts__badge">
                        {t("ClaudeAccountsActive")}
                      </span>
                    )}
                    {isAmbient && (
                      <span className="claude-menu-accounts__badge">
                        {t("ClaudeAccountsSourceAmbient")}
                      </span>
                    )}
                  </span>
                  {pct !== null && (
                    <span className="claude-menu-accounts__bar" aria-hidden>
                      <span
                        className="claude-menu-accounts__bar-fill"
                        style={{ width: `${Math.max(2, Math.min(100, pct))}%` }}
                      />
                    </span>
                  )}
                </div>
                <button
                  type="button"
                  className="claude-menu-accounts__switch"
                  disabled={busy || isActive}
                  onClick={() => void handleSwitch(account.id)}
                >
                  {t("ClaudeAccountsSwitchButton")}
                </button>
              </div>
            </li>
          );
        })}
      </ul>
      </details>
    </>
  );
}

function shrink(id: string): string {
  return id.length <= 12 ? id : `${id.slice(0, 8)}…`;
}
