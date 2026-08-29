use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use uuid::Uuid;

use codexbar::claude_accounts::{
    ClaudeAccount, ClaudeAccountManager, ClaudeAccountManagerError, ClaudeAccountSource,
    ClaudeAccountStore, ClaudeAccountUsageSnapshot, ClaudeSnapshotStore, ClaudeSwitchResult,
    RemovedAccountIdentity, active_account_id, credentials_merge, file_locations,
    reconcile_stored_accounts, usage,
};
use codexbar::core::{ProviderFetchResult, RateWindow};
use codexbar::providers::claude::ClaudeOAuthFetcher;
use codexbar::settings::Settings;

use crate::state::AppState;

use super::*;

// ── Claude multi-account (mirrors codex_accounts.rs's shape) ────────────

const DEFAULT_FETCH_TIMEOUT_SECONDS: u64 = 60;

/// Message returned by every consent-gated write command when
/// `claude_allow_managing_claude_code_accounts` is off (spec: "Consent gate
/// blocks every write path").
const CONSENT_DENIED_MESSAGE: &str =
    "Managing Claude Code accounts is off. Enable it in Settings → Providers → Claude.";

fn claude_accounts_consent() -> bool {
    Settings::load().claude_allow_managing_claude_code_accounts
}

/// All stored + discovered Claude accounts, with the stored list preferred.
pub(crate) fn load_claude_accounts() -> Result<Vec<ClaudeAccount>, String> {
    let store = ClaudeAccountStore::new();
    let (existing, removed) = store.load().map_err(|e| e.to_string())?;

    let manager = ClaudeAccountManager::new();
    let managed = manager
        .discover_managed_accounts(&existing)
        .map_err(|e| e.to_string())?;
    let ambient = manager.discover_ambient_account(&existing);

    // Filter out any account matching a removed-identity BEFORE the merge, so a
    // leftover directory or credential file cannot resurrect a removed account
    // (#14 bug 1). Matched accounts are excluded from the listing only — never
    // deleted here.
    let managed: Vec<ClaudeAccount> = managed
        .into_iter()
        .filter(|account| !removed.iter().any(|r| r.matches(account)))
        .collect();
    let ambient = ambient.filter(|account| !removed.iter().any(|r| r.matches(account)));

    let mut merged: Vec<ClaudeAccount> = managed.clone();
    if let Some(ambient) = ambient {
        if let Some(entry) = merged.iter_mut().find(|account| account.matches(&ambient)) {
            entry.merge_from(&ambient);
        } else {
            merged.push(ambient);
        }
    }

    // Reconcile persisted metadata (nickname, stored timestamps) for managed
    // homes, hydrate stale identities from each dir's own `.claude.json`, and
    // drop managed rows whose dir no longer resolves on disk (#12).
    let mut reconciled: Vec<ClaudeAccount> = reconcile_stored_accounts(
        &existing,
        &managed,
        &file_locations::managed_configs_directory(),
    );

    // Add any newly discovered accounts that are not yet persisted.
    for candidate in &merged {
        if !reconciled.iter().any(|account| account.matches(candidate)) {
            reconciled.push(candidate.clone());
        }
    }

    // Final pass: a stale stored row cannot survive a removal either.
    Ok(reconciled
        .into_iter()
        .filter(|account| !removed.iter().any(|r| r.matches(account)))
        .collect())
}

/// Persist the given accounts to the account store.
pub(crate) fn persist_claude_accounts(accounts: &[ClaudeAccount]) -> Result<(), String> {
    let store = ClaudeAccountStore::new();
    let (_existing, removed) = store.load().map_err(|e| e.to_string())?;
    store
        .save(accounts, Some(&removed))
        .map_err(|e| e.to_string())
}

/// Access tokens for every managed (non-ambient) account, keyed by account
/// id. Consulted by [`active_account_id`]'s access-token fallback (spec:
/// "Fallback match via access-token equality"). Pure file reads only — no
/// `claude` subprocess.
fn managed_access_tokens(accounts: &[ClaudeAccount]) -> HashMap<Uuid, String> {
    accounts
        .iter()
        .filter(|account| account.source == ClaudeAccountSource::ManagedByApp)
        .filter_map(|account| {
            let root = credentials_merge::read_root(&account.claude_config_dir).ok()?;
            let token = root.get("claudeAiOauth")?.get("accessToken")?.as_str()?;
            Some((account.id, token.to_string()))
        })
        .collect()
}

/// Pure mapping from the generic provider fetch result into the per-account
/// stored shape. Duplicated (not imported) from
/// `claude_accounts::usage::usage_snapshot_from_fetch_result`, which is
/// private to that module — this is the IPC layer's own DTO mapping for the
/// ambient refresh lane (design D5), which goes through
/// `ClaudeOAuthFetcher::fetch()` rather than `usage::fetch_snapshot`.
fn claude_usage_snapshot_from_fetch_result(
    result: &ProviderFetchResult,
) -> ClaudeAccountUsageSnapshot {
    let window_snapshot = |window: &RateWindow| {
        codexbar::claude_accounts::UsageWindowSnapshot::new(
            window.used_percent,
            window.resets_at,
            window
                .window_minutes
                .map(|minutes| i64::from(minutes) * 60)
                .unwrap_or(0),
        )
    };
    ClaudeAccountUsageSnapshot {
        email: result.usage.account_email.clone(),
        org_id: None,
        plan: result.usage.login_method.clone(),
        primary_window: Some(window_snapshot(&result.usage.primary)),
        secondary_window: result.usage.secondary.as_ref().map(window_snapshot),
        updated_at: result.usage.updated_at,
    }
}

/// Refresh quota snapshots for every Claude account (ambient + managed) on
/// the same cycle.
///
/// Design decision D5: the account whose directory matches ambient reuses
/// the existing `ClaudeOAuthFetcher::fetch()` path, which transparently
/// refreshes and persists ambient credentials exactly like the plain Claude
/// provider lane does. Every other (genuinely managed) directory uses the
/// per-dir, non-refreshing `usage::fetch_snapshot` path. Exactly one lane is
/// ever capable of rotating a given refresh token, which avoids the
/// refresh-token-reuse race the proposal's Risk 1 warns about (task 2.13).
///
/// Failures are per-account and non-fatal: the ambient provider snapshot and
/// the on-demand `claude_account_fetch` command remain authoritative, and the
/// store keeps the last good snapshot per account.
pub(crate) async fn refresh_claude_account_lanes(
    app: tauri::AppHandle,
    fetch_permits: Arc<tokio::sync::Semaphore>,
) {
    let accounts = match load_claude_accounts() {
        Ok(accounts) => accounts,
        Err(e) => {
            tracing::warn!("claude account lanes: failed to load accounts: {e}");
            return;
        }
    };
    if accounts.is_empty() {
        return;
    }

    let mut handles = Vec::with_capacity(accounts.len());
    for account in accounts {
        let permits = Arc::clone(&fetch_permits);
        handles.push(tokio::spawn(async move {
            let Ok(_permit) = permits.acquire_owned().await else {
                return None;
            };
            let is_ambient = account.source == ClaudeAccountSource::Ambient;
            let fetch_result =
                tokio::time::timeout(Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECONDS), async {
                    if is_ambient {
                        ClaudeOAuthFetcher::new()
                            .fetch()
                            .await
                            .map(|result| claude_usage_snapshot_from_fetch_result(&result))
                    } else {
                        usage::fetch_snapshot(&account.claude_config_dir).await
                    }
                })
                .await;
            match fetch_result {
                Ok(Ok(snapshot)) => Some((account.id, snapshot)),
                Ok(Err(e)) => {
                    tracing::debug!("claude account lane {} failed: {}", account.id, e);
                    None
                }
                Err(_) => {
                    tracing::debug!("claude account lane {} timed out", account.id);
                    None
                }
            }
        }));
    }

    let mut snapshots = ClaudeSnapshotStore::new().load().unwrap_or_default();
    for handle in handles {
        if let Ok(Some((id, snapshot))) = handle.await {
            snapshots.insert(id, snapshot);
        }
    }
    if let Err(e) = ClaudeSnapshotStore::new().save(&snapshots) {
        tracing::warn!("claude account lanes: failed to persist snapshots: {e}");
    }
    events::emit_claude_accounts_updated(&app);
}

// ── Consent-gated write paths ────────────────────────────────────────────
//
// Each `*_gated` function takes `consent` as an explicit argument and checks
// it as the very first statement, before any other disk access (spec:
// "the operation is rejected before touching disk"). The `#[tauri::command]`
// wrappers below read `claude_accounts_consent()` and forward it in, which
// also makes the gate independently unit-testable without a live Tauri
// `AppHandle` or a global settings file.

fn add_claude_account_gated(consent: bool) -> Result<ClaudeAccount, String> {
    if !consent {
        return Err(CONSENT_DENIED_MESSAGE.to_string());
    }
    let account = ClaudeAccountManager::new()
        .add_managed_account(None)
        .map_err(into_user_message)?;

    // G9: un-remove on re-add. Drop every `removed` entry matching the newly
    // added account's identity, otherwise remove-then-`claude auth login`-then-
    // re-add would silently produce a permanently invisible account.
    let store = ClaudeAccountStore::new();
    let (stored, removed) = store.load().map_err(|e| e.to_string())?;
    let kept: Vec<RemovedAccountIdentity> = removed
        .into_iter()
        .filter(|r| !r.matches(&account))
        .collect();
    store
        .save(&stored, Some(&kept))
        .map_err(|e| e.to_string())?;

    Ok(account)
}

fn remove_claude_account_gated(consent: bool, id: &str) -> Result<Vec<ClaudeAccount>, String> {
    if !consent {
        return Err(CONSENT_DENIED_MESSAGE.to_string());
    }
    let manager = ClaudeAccountManager::new();
    let accounts = load_claude_accounts()?;
    let target = accounts
        .iter()
        .find(|account| account.id.to_string() == id)
        .ok_or_else(|| "Claude account not found.".to_string())?;

    manager
        .remove_managed_files_if_owned(target)
        .map_err(into_user_message)?;

    // Record the removed identity so discovery filters it out permanently
    // (#14 bug 1). Build it before `accounts` is consumed below.
    let removed_identity = RemovedAccountIdentity::from_account(target);

    let remaining: Vec<ClaudeAccount> = accounts
        .into_iter()
        .filter(|account| account.id.to_string() != id)
        .collect();

    // Persist `remaining` together with the appended removed-identity list.
    // `persist_claude_accounts` already round-trips `removed` but cannot
    // append, so the remove path calls `store.save` directly.
    let store = ClaudeAccountStore::new();
    let (_stored, mut removed) = store.load().map_err(|e| e.to_string())?;
    removed.push(removed_identity);
    removed.sort_by_key(|r| r.removed_at);
    removed.dedup_by(|a, b| {
        a.claude_config_dir == b.claude_config_dir
            && a.org_id == b.org_id
            && a.email_hint == b.email_hint
    });
    store
        .save(&remaining, Some(&removed))
        .map_err(|e| e.to_string())?;

    // Best-effort: never fails the removal (G7 — mtime-guarded, removal path
    // only).
    let _ = manager.prune_stub_managed_dirs();

    Ok(remaining)
}

fn switch_claude_account_gated(consent: bool, id: &str) -> Result<ClaudeSwitchResult, String> {
    if !consent {
        return Err(CONSENT_DENIED_MESSAGE.to_string());
    }
    let manager = ClaudeAccountManager::new();
    let accounts = load_claude_accounts()?;
    let target = accounts
        .iter()
        .find(|account| account.id.to_string() == id)
        .ok_or_else(|| "Claude account not found.".to_string())?
        .clone();

    manager
        .switch_active_account(&target, &accounts)
        .map_err(into_user_message)
}

#[tauri::command]
pub fn claude_accounts_list() -> Result<Vec<ClaudeAccount>, String> {
    load_claude_accounts()
}

#[tauri::command]
pub async fn claude_account_add(app: tauri::AppHandle) -> Result<ClaudeAccount, String> {
    let consent = claude_accounts_consent();
    let account = tauri::async_runtime::spawn_blocking(move || add_claude_account_gated(consent))
        .await
        .map_err(|e| e.to_string())??;

    if let Err(e) = refresh_persisted_accounts(app) {
        tracing::error!("failed to persist accounts after add: {e}");
    }
    Ok(account)
}

#[tauri::command]
pub fn claude_account_remove(app: tauri::AppHandle, id: String) -> Result<(), String> {
    remove_claude_account_gated(claude_accounts_consent(), &id)?;
    events::emit_settings_changed(&app);
    events::emit_claude_accounts_updated(&app);
    Ok(())
}

#[tauri::command]
pub async fn claude_account_switch(
    app: tauri::AppHandle,
    id: String,
) -> Result<ClaudeSwitchResult, String> {
    let consent = claude_accounts_consent();
    let result =
        tauri::async_runtime::spawn_blocking(move || switch_claude_account_gated(consent, &id))
            .await
            .map_err(|e| e.to_string())??;

    // Materialized ambient account may need persisting.
    if let Some(materialized) = &result.materialized_account {
        let mut accounts = load_claude_accounts()?;
        if let Some(entry) = accounts.iter_mut().find(|a| a.matches(materialized)) {
            entry.merge_from(materialized);
        } else {
            accounts.push(materialized.clone());
        }
        persist_claude_accounts(&accounts)?;
    }

    events::emit_settings_changed(&app);
    events::emit_claude_accounts_updated(&app);
    Ok(result)
}

#[tauri::command]
pub async fn claude_account_fetch(
    app: tauri::AppHandle,
    id: String,
) -> Result<ClaudeAccountUsageSnapshot, String> {
    let accounts = load_claude_accounts()?;
    let target = accounts
        .iter()
        .find(|account| account.id.to_string() == id)
        .ok_or_else(|| "Claude account not found.".to_string())?
        .clone();

    let snapshot = tokio::time::timeout(
        Duration::from_secs(DEFAULT_FETCH_TIMEOUT_SECONDS),
        usage::fetch_snapshot(&target.claude_config_dir),
    )
    .await
    .map_err(|_| "Timed out waiting for the Claude usage API.".to_string())?
    .map_err(|e| e.to_string())?;

    // Persist snapshot to the snapshot store, keyed by account id.
    if let Ok(mut snapshots) = ClaudeSnapshotStore::new().load() {
        snapshots.insert(target.id, snapshot.clone());
        let _ = ClaudeSnapshotStore::new().save(&snapshots);
    }

    if refresh_persisted_accounts(app).is_err() {
        // Non-fatal: the snapshot was still fetched.
    }
    Ok(snapshot)
}

#[tauri::command]
pub fn claude_account_snapshots() -> Result<HashMap<Uuid, ClaudeAccountUsageSnapshot>, String> {
    ClaudeSnapshotStore::new().load().map_err(|e| e.to_string())
}

/// Merge discovered accounts back into the persisted list after identity
/// changes (login/switch) so the store reflects reality.
fn refresh_persisted_accounts(app: tauri::AppHandle) -> Result<(), String> {
    let accounts = load_claude_accounts()?;
    persist_claude_accounts(&accounts)?;
    events::emit_settings_changed(&app);
    Ok(())
}

fn into_user_message(error: ClaudeAccountManagerError) -> String {
    error.to_string()
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeAccountsStateBridge {
    pub accounts: Vec<ClaudeAccount>,
    pub snapshots: HashMap<Uuid, ClaudeAccountUsageSnapshot>,
    /// Id of the account whose identity matches the live ambient Claude Code
    /// state, or `None` when it is absent/unreadable or matches no listed
    /// account. Additive and optional — legacy consumers ignore it.
    pub active_account_id: Option<Uuid>,
}

/// File-based only (spec: "Active-account detection") — no `claude`
/// subprocess is spawned building this state. Split out from the
/// `#[tauri::command]` wrapper so it is unit-testable without a live
/// `tauri::State`.
fn build_claude_accounts_state() -> Result<ClaudeAccountsStateBridge, String> {
    let accounts = load_claude_accounts()?;
    let identity = ClaudeAccountManager::new().load_active_identity();
    let managed_tokens = managed_access_tokens(&accounts);
    let active = active_account_id(&accounts, &identity, &managed_tokens);
    Ok(ClaudeAccountsStateBridge {
        accounts,
        snapshots: claude_account_snapshots()?,
        active_account_id: active,
    })
}

#[tauri::command]
pub fn get_claude_accounts_state(
    state: tauri::State<'_, Mutex<AppState>>,
) -> Result<ClaudeAccountsStateBridge, String> {
    let _guard = state.lock().map_err(|e| e.to_string())?;
    build_claude_accounts_state()
}

#[cfg(test)]
mod tests {
    use super::*;
    use codexbar::claude_accounts::file_locations;
    use std::fs;

    fn sample_account(dir: std::path::PathBuf, source: ClaudeAccountSource) -> ClaudeAccount {
        ClaudeAccount::new(
            Uuid::new_v4(),
            None,
            Some("user@example.com".to_string()),
            Some("org-acct".to_string()),
            Some("Acme".to_string()),
            Some("max".to_string()),
            dir,
            source,
            codexbar::claude_accounts::utc_now(),
            codexbar::claude_accounts::utc_now(),
            Some(codexbar::claude_accounts::utc_now()),
        )
    }

    fn write_credentials(dir: &std::path::Path, access_token: &str) {
        fs::create_dir_all(dir).unwrap();
        let payload = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access_token,
                "refreshToken": format!("refresh-{access_token}"),
            },
            "mcpOAuth": {"some-server": {"accessToken": "keepme"}},
        });
        fs::write(
            credentials_merge::credentials_file_path(dir),
            serde_json::to_vec_pretty(&payload).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn into_user_message_preserves_friendly_text() {
        assert_eq!(
            into_user_message(ClaudeAccountManagerError::Message(
                "The `claude` command could not be found.".to_string()
            )),
            "The `claude` command could not be found."
        );
    }

    #[test]
    fn sample_account_serializes_camel_case() {
        let dir = std::path::PathBuf::from("/tmp/fake-claude-home");
        let json =
            serde_json::to_value(sample_account(dir, ClaudeAccountSource::ManagedByApp)).unwrap();
        assert!(json.get("claudeConfigDir").is_some());
        assert!(json.get("orgId").is_some());
    }

    #[test]
    fn accounts_state_bridge_serializes_active_account_id_camel_case() {
        let account = sample_account(
            std::path::PathBuf::from("/tmp/fake-claude-home"),
            ClaudeAccountSource::ManagedByApp,
        );
        let bridge = ClaudeAccountsStateBridge {
            accounts: vec![account.clone()],
            snapshots: HashMap::new(),
            active_account_id: Some(account.id),
        };
        let json = serde_json::to_value(&bridge).unwrap();
        assert_eq!(
            json.get("activeAccountId").and_then(|v| v.as_str()),
            Some(account.id.to_string().as_str())
        );

        let none = ClaudeAccountsStateBridge {
            accounts: vec![],
            snapshots: HashMap::new(),
            active_account_id: None,
        };
        assert!(serde_json::to_value(&none).unwrap()["activeAccountId"].is_null());
    }

    /// Regression for the Codex analog (issue #1): an account discovered on
    /// disk but never written to `accounts.json` must keep the same id
    /// between two `load_claude_accounts` calls, so the id the UI listed
    /// still resolves on the follow-up switch/remove/fetch IPC (task 2.7).
    #[test]
    fn discovered_account_id_resolves_on_a_later_call() {
        let unique = Uuid::new_v4().to_string();
        let mut root = std::env::temp_dir();
        root.push(format!("codexbar-claude-cmd-test-{unique}"));
        let app_support = root.join("app-support");
        let ambient_dir = root.join("dot-claude");
        let claude_json = root.join(".claude.json");
        fs::create_dir_all(&app_support).unwrap();

        file_locations::with_app_support_directory(app_support.clone());
        file_locations::with_ambient_claude_config_dir(ambient_dir.clone());
        file_locations::with_ambient_claude_json_path(claude_json.clone());

        write_credentials(&ambient_dir, "ambient-access-token");
        fs::write(
            &claude_json,
            serde_json::to_vec_pretty(&serde_json::json!({
                "oauthAccount": {
                    "emailAddress": "ambient@example.com",
                    "organizationUuid": "org-never-persisted",
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let first = load_claude_accounts().expect("first list");
        let listed_id = first
            .iter()
            .find(|a| a.claude_config_dir == ambient_dir)
            .expect("ambient account listed")
            .id
            .to_string();

        let second = load_claude_accounts().expect("second list");
        let resolved = second.iter().any(|a| a.id.to_string() == listed_id);

        file_locations::clear_app_support_directory_override();
        file_locations::clear_ambient_claude_config_dir_override();
        file_locations::clear_ambient_claude_json_path_override();
        let _ = fs::remove_dir_all(&root);

        assert!(
            resolved,
            "discovered account id did not resolve on a later call"
        );
    }

    // ── Consent gate: task 2.4 ───────────────────────────────────────────

    #[test]
    fn add_is_rejected_and_performs_no_filesystem_writes_when_consent_is_off() {
        let dir = tempfile::tempdir().unwrap();
        file_locations::with_app_support_directory(dir.path().to_path_buf());

        let result = add_claude_account_gated(false);

        let managed_dir_entries = fs::read_dir(dir.path().join("managed-configs"))
            .map(|entries| entries.count())
            .unwrap_or(0);

        file_locations::clear_app_support_directory_override();

        assert_eq!(result.unwrap_err(), CONSENT_DENIED_MESSAGE);
        assert_eq!(
            managed_dir_entries, 0,
            "add must not create a managed config directory when consent is off"
        );
    }

    #[test]
    fn remove_is_rejected_and_leaves_the_managed_directory_untouched_when_consent_is_off() {
        let dir = tempfile::tempdir().unwrap();
        file_locations::with_app_support_directory(dir.path().to_path_buf());

        let owned = file_locations::managed_configs_directory()
            .join("11111111-1111-1111-1111-111111111111");
        write_credentials(&owned, "token");
        let account = sample_account(owned.clone(), ClaudeAccountSource::ManagedByApp);
        let store = ClaudeAccountStore::new();
        store.save(std::slice::from_ref(&account), None).unwrap();

        let result = remove_claude_account_gated(false, &account.id.to_string());

        let still_exists = owned.exists();
        let accounts_after = ClaudeAccountStore::new().load_accounts().unwrap();

        file_locations::clear_app_support_directory_override();

        assert_eq!(result.unwrap_err(), CONSENT_DENIED_MESSAGE);
        assert!(
            still_exists,
            "remove must not delete the managed directory when consent is off"
        );
        assert_eq!(
            accounts_after.len(),
            1,
            "remove must not persist a shorter account list when consent is off"
        );
    }

    #[test]
    fn switch_is_rejected_and_leaves_ambient_credentials_untouched_when_consent_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        file_locations::with_app_support_directory(root.to_path_buf());
        let ambient_dir = root.join(".claude");
        file_locations::with_ambient_claude_config_dir(ambient_dir.clone());

        write_credentials(&ambient_dir, "old-ambient-token");
        let target_dir = file_locations::managed_configs_directory().join("target");
        write_credentials(&target_dir, "new-managed-token");
        let target_account = sample_account(target_dir, ClaudeAccountSource::ManagedByApp);
        let store = ClaudeAccountStore::new();
        store
            .save(std::slice::from_ref(&target_account), None)
            .unwrap();

        let result = switch_claude_account_gated(false, &target_account.id.to_string());

        let ambient_root = credentials_merge::read_root(&ambient_dir).unwrap();
        let backups_empty = fs::read_dir(file_locations::credentials_backups_directory())
            .map(|entries| entries.count() == 0)
            .unwrap_or(true);

        file_locations::clear_app_support_directory_override();
        file_locations::clear_ambient_claude_config_dir_override();

        assert_eq!(result.unwrap_err(), CONSENT_DENIED_MESSAGE);
        assert_eq!(
            ambient_root["claudeAiOauth"]["accessToken"], "old-ambient-token",
            "switch must not touch ambient credentials when consent is off"
        );
        assert!(
            backups_empty,
            "switch must not create a credentials backup when consent is off"
        );
    }

    #[test]
    fn switch_proceeds_past_the_gate_when_consent_is_on() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        file_locations::with_app_support_directory(root.to_path_buf());
        let ambient_dir = root.join(".claude");
        file_locations::with_ambient_claude_config_dir(ambient_dir.clone());

        write_credentials(&ambient_dir, "old-ambient-token");
        let target_dir = file_locations::managed_configs_directory().join("target");
        write_credentials(&target_dir, "new-managed-token");
        let target_account = sample_account(target_dir, ClaudeAccountSource::ManagedByApp);

        let result = switch_claude_account_gated(true, &target_account.id.to_string());

        file_locations::clear_app_support_directory_override();
        file_locations::clear_ambient_claude_config_dir_override();

        // Consent granted: the gate does not block, so the lookup runs and
        // fails for the real reason (id not in the persisted/discovered
        // list) instead of the consent message.
        assert_eq!(result.unwrap_err(), "Claude account not found.");
    }

    // ── D5 refresh-lane mapping ───────────────────────────────────────────

    #[test]
    fn claude_usage_snapshot_maps_window_minutes_to_limit_window_seconds() {
        let primary = RateWindow::with_details(10.0, Some(300), None, None);
        let usage = codexbar::core::UsageSnapshot::new(primary).with_email("a@example.com");
        let result = ProviderFetchResult::new(usage, "oauth");

        let snapshot = claude_usage_snapshot_from_fetch_result(&result);
        assert_eq!(
            snapshot
                .primary_window
                .as_ref()
                .unwrap()
                .limit_window_seconds,
            300 * 60
        );
        assert_eq!(snapshot.email.as_deref(), Some("a@example.com"));
    }

    // ── #14: removed-identity filter (R7) + un-remove on re-add (R7b) ─────
    //
    // NOTE: these live in the Tauri crate's `#[cfg(test)]` and cannot be run
    // on WSL2 (the crate links GTK/atk via pkg-config). They run on CI and on
    // the maintainer's Windows host. The equivalent library-crate coverage of
    // the deletion / gate primitives lives in
    // `rust/src/claude_accounts/account_manager.rs`.

    fn write_managed_claude_json(dir: &std::path::Path, email: &str, org_uuid: &str) {
        fs::create_dir_all(dir).unwrap();
        let payload = serde_json::json!({
            "oauthAccount": {
                "emailAddress": email,
                "organizationUuid": org_uuid,
                "organizationName": format!("{org_uuid}-name"),
            }
        });
        fs::write(
            dir.join(".claude.json"),
            serde_json::to_vec_pretty(&payload).unwrap(),
        )
        .unwrap();
    }

    // [R7] once an account is in `removed_accounts`, a matching managed
    // directory left on disk (e.g. from a later `claude auth login`) is
    // excluded from the listing — it never resurrects.
    #[test]
    fn removed_identity_keeps_account_hidden_despite_a_leftover_dir() {
        let dir = tempfile::tempdir().unwrap();
        file_locations::with_app_support_directory(dir.path().to_path_buf());
        file_locations::with_ambient_claude_config_dir(dir.path().join(".claude"));
        file_locations::with_ambient_claude_json_path(dir.path().join(".claude.json"));

        let leftover = file_locations::managed_configs_directory()
            .join("11111111-1111-1111-1111-111111111111");
        write_credentials(&leftover, "leftover-token");
        write_managed_claude_json(&leftover, "gone@example.com", "org-gone");

        let removed_marker = RemovedAccountIdentity::from_account(&sample_account(
            leftover.clone(),
            ClaudeAccountSource::ManagedByApp,
        ));
        let mut removed_marker = removed_marker;
        removed_marker.email_hint = Some("gone@example.com".to_string());
        removed_marker.org_id = Some("org-gone".to_string());

        ClaudeAccountStore::new()
            .save(&[], Some(&[removed_marker]))
            .unwrap();

        let listed = load_claude_accounts().unwrap();

        let still_on_disk = leftover.exists();
        file_locations::clear_app_support_directory_override();
        file_locations::clear_ambient_claude_config_dir_override();
        file_locations::clear_ambient_claude_json_path_override();

        assert!(
            listed
                .iter()
                .all(|a| a.org_id.as_deref() != Some("org-gone")),
            "a removed identity must not reappear in the listing"
        );
        assert!(
            still_on_disk,
            "the leftover directory is excluded from the listing, not deleted here"
        );
    }

    // [R7b] purging the removed marker (what `add_claude_account_gated` does
    // after a successful re-add) makes the identity visible again.
    #[test]
    fn purging_the_removed_marker_makes_the_account_visible_again() {
        let dir = tempfile::tempdir().unwrap();
        file_locations::with_app_support_directory(dir.path().to_path_buf());
        file_locations::with_ambient_claude_config_dir(dir.path().join(".claude"));
        file_locations::with_ambient_claude_json_path(dir.path().join(".claude.json"));

        let managed = file_locations::managed_configs_directory()
            .join("22222222-2222-2222-2222-222222222222");
        write_credentials(&managed, "back-token");
        write_managed_claude_json(&managed, "back@example.com", "org-back");
        let account = sample_account(managed.clone(), ClaudeAccountSource::ManagedByApp);
        let mut account = account;
        account.email_hint = Some("back@example.com".to_string());
        account.org_id = Some("org-back".to_string());

        let store = ClaudeAccountStore::new();
        store
            .save(
                std::slice::from_ref(&account),
                Some(&[RemovedAccountIdentity::from_account(&account)]),
            )
            .unwrap();
        assert!(
            load_claude_accounts()
                .unwrap()
                .iter()
                .all(|a| a.org_id.as_deref() != Some("org-back")),
            "precondition: the marker hides the account"
        );

        // G9: drop every removed marker matching the re-added identity.
        let (stored, removed) = store.load().unwrap();
        let kept: Vec<RemovedAccountIdentity> = removed
            .into_iter()
            .filter(|r| !r.matches(&account))
            .collect();
        store.save(&stored, Some(&kept)).unwrap();

        let listed = load_claude_accounts().unwrap();
        file_locations::clear_app_support_directory_override();
        file_locations::clear_ambient_claude_config_dir_override();
        file_locations::clear_ambient_claude_json_path_override();

        assert!(
            listed
                .iter()
                .any(|a| a.org_id.as_deref() == Some("org-back")),
            "after the marker is purged the account is visible again"
        );
    }
}
