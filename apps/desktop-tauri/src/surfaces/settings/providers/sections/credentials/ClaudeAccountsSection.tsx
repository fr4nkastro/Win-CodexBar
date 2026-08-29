import { useCallback, useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import type {
  ClaudeAccount,
  ClaudeAccountsStateBridge,
  ClaudeAccountUsageSnapshot,
} from "../../../../../types/bridge";
import type { LocaleKey } from "../../../../../i18n/keys";
import {
  claudeAccountAdd,
  claudeAccountFetch,
  claudeAccountRemove,
  claudeAccountSwitch,
  getClaudeAccountsState,
  getSettingsSnapshot,
  refreshProviders,
} from "../../../../../lib/tauri";

interface Props {
  t: (key: LocaleKey) => string;
}

/**
 * Inline "Claude Accounts" surface shown inside the Settings → Providers →
 * Claude detail pane (claude-multi-account, 1:1 port of the Codex accounts
 * settings section, minus the Codex-Desktop-specific session restart flow —
 * Claude Code has no analogous MSIX desktop session to restore).
 *
 * Reads the shared account + snapshot store via `get_claude_accounts_state`
 * and drives the `claude_account_*` IPC surface: add (login into a managed
 * home), switch the active ambient identity, refresh per-account usage, and
 * remove managed homes. Every write path is rejected server-side unless the
 * `claudeAllowManagingClaudeCodeAccounts` consent flag is on — the caller
 * (`ProviderDetailPane`) is responsible for gating this section's visibility
 * on that flag.
 */
export function ClaudeAccountsSection({ t }: Props) {
  const [consentGranted, setConsentGranted] = useState<boolean | null>(null);
  const [accounts, setAccounts] = useState<ClaudeAccount[]>([]);
  const [snapshots, setSnapshots] = useState<
    Record<string, ClaudeAccountUsageSnapshot>
  >({});
  const [activeAccountId, setActiveAccountId] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [switchSucceeded, setSwitchSucceeded] = useState(false);

  // Gated on the `claudeAllowManagingClaudeCodeAccounts` consent flag: the
  // whole section stays hidden until the user opts in via `ClaudeCreds`'s
  // toggle, even though listing accounts is not itself write-gated.
  useEffect(() => {
    let cancelled = false;
    getSettingsSnapshot()
      .then((s) => {
        if (!cancelled) {
          setConsentGranted(s.claudeAllowManagingClaudeCodeAccounts ?? false);
        }
      })
      .catch((err: unknown) => {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : String(err));
          setConsentGranted(false);
        }
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const load = useCallback(async () => {
    setBusy(true);
    setError(null);
    try {
      const next: ClaudeAccountsStateBridge = await getClaudeAccountsState();
      setAccounts(next.accounts);
      setSnapshots(next.snapshots);
      setActiveAccountId(next.activeAccountId ?? null);
      setLoaded(true);
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }, []);

  useEffect(() => {
    if (consentGranted) void load();
  }, [consentGranted, load]);

  // Live-refresh after the provider engine runs the per-account lanes so the
  // panel stays current.
  useEffect(() => {
    if (!consentGranted) return;
    let cancelled = false;
    const unlistenPromise = listen("claude-accounts-updated", () => {
      if (!cancelled) void load();
    });
    return () => {
      cancelled = true;
      void unlistenPromise.then((fn) => fn());
    };
  }, [consentGranted, load]);

  const handleAdd = async () => {
    setBusy(true);
    setError(null);
    setSwitchSucceeded(false);
    try {
      await claudeAccountAdd();
      await load();
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  const handleSwitch = async (id: string) => {
    setBusy(true);
    setError(null);
    setSwitchSucceeded(false);
    try {
      await claudeAccountSwitch(id);
      setSwitchSucceeded(true);
      await load();
      // Mirror the tray menu: make the tray icon and Claude provider card
      // reflect the now-active identity without waiting for a refresh tick.
      void refreshProviders().catch(() => {});
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  const handleFetch = async (id: string) => {
    setBusy(true);
    setError(null);
    try {
      const snapshot = await claudeAccountFetch(id);
      setSnapshots((prev) => ({ ...prev, [id]: snapshot }));
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  const handleRemove = async (id: string) => {
    setBusy(true);
    setError(null);
    setSwitchSucceeded(false);
    try {
      await claudeAccountRemove(id);
      await load();
    } catch (err: unknown) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  if (!consentGranted || !loaded) {
    return null;
  }

  return (
    <section className="provider-detail-section claude-accounts">
      <div className="provider-detail-section__header">
        <h4>{t("ClaudeAccountsTitle")}</h4>
        {accounts.length === 0 && (
          <button
            type="button"
            className="credential-btn credential-btn--primary"
            disabled={busy}
            onClick={() => void handleAdd()}
          >
            {t("ClaudeAccountsAddButton")}
          </button>
        )}
      </div>
      <p className="settings-section__hint">{t("ClaudeAccountsHint")}</p>
      <p className="settings-section__hint">{t("ClaudeAccountsCloseWarning")}</p>

      {error && (
        <div className="provider-detail-error" role="alert">
          {error}
        </div>
      )}

      {switchSucceeded && (
        <div className="provider-detail-note" role="status">
          {t("ClaudeSwitchSuccess")}
        </div>
      )}

      {accounts.length === 0 ? (
        <p className="credential-empty">{t("ClaudeAccountsEmpty")}</p>
      ) : (
        <>
          <ul className="credential-list claude-accounts-list">
            {accounts.map((account) => {
              const snapshot = snapshots[account.id];
              const isActive = account.id === activeAccountId;
              return (
                <li
                  key={account.id}
                  className="credential-card claude-accounts-card"
                >
                  <div className="credential-card__header">
                    <div className="credential-card__info">
                      <strong>
                        {account.nickname ??
                          account.emailHint ??
                          account.orgName ??
                          shrink(account.id)}
                      </strong>
                      <span className="credential-card__meta">
                        <span className="credential-card__badge credential-card__badge--set">
                          {account.source === "ambient"
                            ? t("ClaudeAccountsSourceAmbient")
                            : t("ClaudeAccountsSourceManaged")}
                        </span>
                        {isActive && (
                          <span className="credential-card__badge credential-card__badge--set">
                            {t("ClaudeAccountsActive")}
                          </span>
                        )}
                        {snapshot ? (
                          <ClaudeUsagePill snapshot={snapshot} t={t} />
                        ) : (
                          <span className="credential-card__date">
                            {t("ClaudeAccountsUsageUnavailable")}
                          </span>
                        )}
                      </span>
                    </div>
                    <div className="credential-card__actions">
                      <button
                        type="button"
                        className="credential-btn credential-btn--secondary"
                        disabled={busy}
                        onClick={() => void handleFetch(account.id)}
                      >
                        {t("ClaudeAccountsFetchButton")}
                      </button>
                      <button
                        type="button"
                        className="credential-btn credential-btn--primary"
                        disabled={busy || isActive}
                        onClick={() => void handleSwitch(account.id)}
                      >
                        {t("ClaudeAccountsSwitchButton")}
                      </button>
                      <button
                        type="button"
                        className="credential-btn credential-btn--danger"
                        disabled={busy}
                        onClick={() => void handleRemove(account.id)}
                      >
                        {t("ClaudeAccountsRemoveButton")}
                      </button>
                    </div>
                  </div>
                </li>
              );
            })}
          </ul>
          <div className="claude-accounts-add">
            <button
              type="button"
              className="credential-btn credential-btn--primary"
              disabled={busy}
              onClick={() => void handleAdd()}
            >
              {t("ClaudeAccountsAddButton")}
            </button>
          </div>
        </>
      )}
    </section>
  );
}

function shrink(id: string): string {
  return id.length <= 12 ? id : `${id.slice(0, 8)}…`;
}

function ClaudeUsagePill({
  snapshot,
  t,
}: {
  snapshot: ClaudeAccountUsageSnapshot;
  t: (key: LocaleKey) => string;
}) {
  const window = snapshot.primaryWindow ?? snapshot.secondaryWindow;
  const percent = window ? Math.round(window.usedPercent) : null;
  const plan = snapshot.plan ?? "";
  const label = [plan, percent !== null ? `${percent}%` : null]
    .filter(Boolean)
    .join(" · ");
  return (
    <span className="claude-usage">
      {label || t("ClaudeAccountsUsageUnavailable")}
    </span>
  );
}
