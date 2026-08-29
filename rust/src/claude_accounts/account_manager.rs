//! Claude account management: discovery, authentication, switching, removal.
//!
//! Structurally mirrors `codex_accounts::account_manager`, minus the Codex
//! Desktop MSIX session preservation (Claude Code has no equivalent). Manages
//! isolated managed config directories under `managed-configs/`, discovers
//! the ambient identity, and switches the active identity by merging only the
//! `claudeAiOauth` key into ambient `.credentials.json` (design decision D2).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use thiserror::Error;
use uuid::Uuid;

use super::credentials_merge::{self, CredentialsMergeError};
use super::file_locations::{
    ambient_claude_config_dir, ambient_claude_json_path, credentials_backups_directory,
    ensure_directories, managed_configs_directory,
};
use super::identity::{
    AmbientClaudeIdentity, AmbientOauthAccount, ClaudeIdentity, parse_auth_status_json,
    stable_discovered_id,
};
use super::login_runner::{ClaudeLoginOutcome, ClaudeLoginRunner, ManagedLoginProcess};
use super::models::{ClaudeAccount, ClaudeAccountSource, utc_now};

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Friendly account manager error.
#[derive(Debug, Error)]
pub enum ClaudeAccountManagerError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    CredentialsMerge(#[from] CredentialsMergeError),
}

/// Result of switching the active account.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeSwitchResult {
    pub materialized_account: Option<ClaudeAccount>,
    pub backup_path: Option<PathBuf>,
    pub ambient_account: Option<ClaudeAccount>,
}

/// Discovers, authenticates, and switches Claude accounts.
#[derive(Debug, Default)]
pub struct ClaudeAccountManager;

impl ClaudeAccountManager {
    pub fn new() -> Self {
        Self
    }

    /// Start a `claude auth login --claudeai` into a fresh managed config dir.
    pub fn add_managed_account(
        &self,
        handle: Option<&ManagedLoginProcess>,
    ) -> Result<ClaudeAccount, ClaudeAccountManagerError> {
        ensure_directories()?;
        let dir = managed_configs_directory().join(Uuid::new_v4().to_string());
        fs::create_dir_all(&dir)?;

        match self.authenticate_account(&dir, handle) {
            Ok(account) => Ok(account),
            Err(error) => {
                let _ = fs::remove_dir_all(&dir);
                Err(error)
            }
        }
    }

    /// Remove app-owned managed config directories matching this account.
    ///
    /// Threat Matrix — Destructive file writes: refuses to remove anything
    /// that does not `canonicalize` + `strip_prefix` inside
    /// `managed_configs_directory()`, guarding against a symlink or a
    /// corrupted `claude_config_dir` pointing outside the app's own root.
    pub fn remove_managed_files_if_owned(
        &self,
        account: &ClaudeAccount,
    ) -> Result<(), ClaudeAccountManagerError> {
        if !account.source.owns_files() {
            return Ok(());
        }

        let root = fs::canonicalize(managed_configs_directory())
            .unwrap_or_else(|_| managed_configs_directory());
        let target = std::path::absolute(&account.claude_config_dir)
            .unwrap_or_else(|_| account.claude_config_dir.clone());
        let resolved = fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
        let relative = resolved.strip_prefix(&root).map_err(|_| {
            ClaudeAccountManagerError::Message(
                "This path is not an app-managed config directory.".to_string(),
            )
        })?;
        if relative.as_os_str().is_empty() {
            return Err(ClaudeAccountManagerError::Message(
                "Refusing to remove the managed-configs root.".to_string(),
            ));
        }
        if target.exists() {
            fs::remove_dir_all(&target)?;
        }
        Ok(())
    }

    /// Discover managed config directories and merge them against the stored
    /// accounts.
    ///
    /// Claude's `.credentials.json` alone carries no email/org (unlike
    /// Codex's JWT-bearing `auth.json`), so a managed directory's rich
    /// identity is only recoverable from the already-stored record for that
    /// same directory (captured once via `claude auth status --json` at
    /// add-time); a directory that has fallen out of the store entirely can
    /// only be re-identified by its (stable) directory name.
    pub fn discover_managed_accounts(
        &self,
        existing: &[ClaudeAccount],
    ) -> Result<Vec<ClaudeAccount>, ClaudeAccountManagerError> {
        ensure_directories()?;
        let mut discovered = Vec::new();
        let mut entries: Vec<PathBuf> = fs::read_dir(managed_configs_directory())?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir())
            .collect();
        entries.sort_by_key(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        });
        for dir in entries {
            if let Some(account) = self.discovered_managed_account(&dir, existing) {
                discovered.push(account);
            }
        }
        Ok(discovered)
    }

    /// Discover the ambient Claude Code account from `~/.claude.json`'s
    /// `oauthAccount` block and the ambient `.credentials.json`.
    pub fn discover_ambient_account(&self, existing: &[ClaudeAccount]) -> Option<ClaudeAccount> {
        let dir = ambient_claude_config_dir();
        if !credentials_merge::credentials_file_path(&dir).exists() {
            return None;
        }

        let oauth_account = read_ambient_oauth_account();
        let subscription_type = read_claude_ai_oauth_str_field(&dir, "subscriptionType");
        let identity = ClaudeIdentity {
            email: oauth_account.as_ref().and_then(|o| o.email_address.clone()),
            org_id: oauth_account
                .as_ref()
                .and_then(|o| o.organization_uuid.clone()),
            org_name: None,
            subscription_type,
        };
        if identity.email.is_none() && identity.org_id.is_none() {
            return None;
        }

        let candidate = candidate_account(identity.clone(), &dir, ClaudeAccountSource::Ambient);
        let matched = existing.iter().find(|account| candidate.matches(account));
        let discovered_at = directory_timestamp(&dir);
        Some(build_discovered_account(
            matched,
            identity,
            dir,
            ClaudeAccountSource::Ambient,
            discovered_at,
        ))
    }

    /// File-based ambient identity for active-account detection (spec:
    /// "Active-account detection" — no `claude` subprocess is spawned here).
    pub fn load_active_identity(&self) -> AmbientClaudeIdentity {
        AmbientClaudeIdentity {
            oauth_account: read_ambient_oauth_account(),
            access_token: read_claude_ai_oauth_str_field(
                &ambient_claude_config_dir(),
                "accessToken",
            ),
        }
    }

    /// Switch the ambient identity to `target`, materializing the previous
    /// ambient account as managed first, then merging only `claudeAiOauth`.
    pub fn switch_active_account(
        &self,
        target: &ClaudeAccount,
        existing: &[ClaudeAccount],
    ) -> Result<ClaudeSwitchResult, ClaudeAccountManagerError> {
        ensure_directories()?;

        if !credentials_merge::credentials_file_path(&target.claude_config_dir).exists() {
            return Err(ClaudeAccountManagerError::Message(
                "The selected account does not contain `.credentials.json`.".to_string(),
            ));
        }

        let ambient_dir = ambient_claude_config_dir();
        let ambient_account = self.discover_ambient_account(existing);
        let mut materialized_account: Option<ClaudeAccount> = None;
        if let Some(ambient) = &ambient_account {
            let is_ambient = ambient.source == ClaudeAccountSource::Ambient;
            if is_ambient && !ambient.matches(target) {
                materialized_account = Some(self.materialize_as_managed(ambient)?);
            }
        }

        fs::create_dir_all(&ambient_dir)?;
        let backup_path = self.backup_ambient_credentials(&ambient_dir)?;

        let mut ambient_root = if credentials_merge::credentials_file_path(&ambient_dir).exists() {
            credentials_merge::read_root(&ambient_dir)?
        } else {
            serde_json::json!({})
        };
        let source_root = credentials_merge::read_root(&target.claude_config_dir)?;
        credentials_merge::merge_claude_ai_oauth(&mut ambient_root, &source_root)?;
        credentials_merge::write_root(&ambient_dir, &ambient_root)?;

        Ok(ClaudeSwitchResult {
            materialized_account,
            backup_path,
            ambient_account: self.discover_ambient_account(existing),
        })
    }

    /// Copy the ambient account's whole credentials file into a fresh
    /// app-managed config directory (unlike `switch_active_account`'s
    /// targeted key merge, materialization preserves the ambient file as-is,
    /// including `mcpOAuth`, since it becomes its own independent account).
    pub fn materialize_as_managed(
        &self,
        account: &ClaudeAccount,
    ) -> Result<ClaudeAccount, ClaudeAccountManagerError> {
        ensure_directories()?;

        let source_path = credentials_merge::credentials_file_path(&account.claude_config_dir);
        if !source_path.exists() {
            return Err(ClaudeAccountManagerError::Message(
                "The current active account does not contain `.credentials.json`.".to_string(),
            ));
        }

        let destination_dir = managed_configs_directory().join(Uuid::new_v4().to_string());
        fs::create_dir_all(&destination_dir)?;
        fs::copy(
            &source_path,
            credentials_merge::credentials_file_path(&destination_dir),
        )?;

        let now = utc_now();
        Ok(ClaudeAccount::new(
            account.id,
            account.nickname.clone(),
            account.email_hint.clone(),
            account.org_id.clone(),
            account.org_name.clone(),
            account.subscription_type.clone(),
            destination_dir,
            ClaudeAccountSource::ManagedByApp,
            account.created_at,
            now,
            Some(account.last_authenticated_at.unwrap_or(now)),
        ))
    }

    fn backup_ambient_credentials(
        &self,
        ambient_dir: &Path,
    ) -> Result<Option<PathBuf>, ClaudeAccountManagerError> {
        ensure_directories()?;
        let creds_path = credentials_merge::credentials_file_path(ambient_dir);
        if !creds_path.exists() {
            return Ok(None);
        }
        let backup_path = credentials_backups_directory()
            .join(format!("ambient-credentials-{}.json", timestamp_slug()));
        fs::copy(&creds_path, &backup_path)?;
        Ok(Some(backup_path))
    }

    fn authenticate_account(
        &self,
        dir: &Path,
        handle: Option<&ManagedLoginProcess>,
    ) -> Result<ClaudeAccount, ClaudeAccountManagerError> {
        let result = ClaudeLoginRunner::run(dir, Duration::from_secs(180), handle);

        match &result.outcome {
            ClaudeLoginOutcome::Cancelled => {
                return Err(ClaudeAccountManagerError::Message(
                    "Account setup cancelled.".to_string(),
                ));
            }
            ClaudeLoginOutcome::MissingBinary => {
                return Err(ClaudeAccountManagerError::Message(
                    "The `claude` command could not be found.".to_string(),
                ));
            }
            ClaudeLoginOutcome::TimedOut(_) => {
                return Err(ClaudeAccountManagerError::Message(
                    "The Claude sign-in flow timed out.".to_string(),
                ));
            }
            ClaudeLoginOutcome::LaunchFailed(output) => {
                return Err(ClaudeAccountManagerError::Message(format!(
                    "Failed to start the Claude sign-in flow: {output}"
                )));
            }
            ClaudeLoginOutcome::Failed(output) => {
                return Err(ClaudeAccountManagerError::Message(format!(
                    "The Claude sign-in flow did not complete.\n{output}"
                )));
            }
            ClaudeLoginOutcome::Success(_) => {}
        }

        if !credentials_merge::credentials_file_path(dir).exists() {
            return Err(ClaudeAccountManagerError::Message(
                "Sign-in completed, but no credentials were written for this account.".to_string(),
            ));
        }

        // `claude auth status --json` is invoked exactly once here, scoped to
        // the new account's own directory, to capture its identity (spec:
        // "runs only at add-time").
        let identity = self.capture_identity_via_cli(dir)?;
        if identity.email.is_none() && identity.org_id.is_none() {
            return Err(ClaudeAccountManagerError::Message(
                "Sign-in completed, but the account identity could not be read.".to_string(),
            ));
        }

        let now = utc_now();
        Ok(ClaudeAccount::new(
            Uuid::new_v4(),
            None,
            identity.email,
            identity.org_id,
            identity.org_name,
            identity.subscription_type,
            dir.to_path_buf(),
            ClaudeAccountSource::ManagedByApp,
            now,
            now,
            Some(now),
        ))
    }

    fn capture_identity_via_cli(
        &self,
        dir: &Path,
    ) -> Result<ClaudeIdentity, ClaudeAccountManagerError> {
        let binary = ClaudeLoginRunner::locate_claude_binary().ok_or_else(|| {
            ClaudeAccountManagerError::Message(
                "The `claude` command could not be found.".to_string(),
            )
        })?;

        let mut command = Command::new(binary);
        command
            .args(["auth", "status", "--json"])
            .env("CLAUDE_CONFIG_DIR", dir);
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        let output = command.output()?;
        if !output.status.success() {
            return Err(ClaudeAccountManagerError::Message(
                "`claude auth status --json` did not complete successfully.".to_string(),
            ));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        parse_auth_status_json(&text)
            .map_err(|error| ClaudeAccountManagerError::Message(error.to_string()))
    }

    fn discovered_managed_account(
        &self,
        dir: &Path,
        existing: &[ClaudeAccount],
    ) -> Option<ClaudeAccount> {
        if !dir.is_dir() {
            return None;
        }
        if !credentials_merge::credentials_file_path(dir).exists() {
            return None;
        }

        let standardized_dir = standardized(dir);
        let existing_for_dir = existing
            .iter()
            .find(|account| account.standardized_config_dir() == standardized_dir);
        let subscription_type = read_claude_ai_oauth_str_field(dir, "subscriptionType")
            .or_else(|| existing_for_dir.and_then(|account| account.subscription_type.clone()));
        let identity = ClaudeIdentity {
            email: existing_for_dir.and_then(|account| account.email_hint.clone()),
            org_id: existing_for_dir.and_then(|account| account.org_id.clone()),
            org_name: existing_for_dir.and_then(|account| account.org_name.clone()),
            subscription_type,
        };

        let discovered_at = directory_timestamp(dir);
        let candidate = candidate_account(identity.clone(), dir, ClaudeAccountSource::ManagedByApp);
        let matched = existing
            .iter()
            .find(|account| candidate.matches(account))
            .or(existing_for_dir);
        Some(build_discovered_account(
            matched,
            identity,
            dir.to_path_buf(),
            ClaudeAccountSource::ManagedByApp,
            discovered_at,
        ))
    }
}

fn candidate_account(
    identity: ClaudeIdentity,
    dir: &Path,
    source: ClaudeAccountSource,
) -> ClaudeAccount {
    let id = stable_discovered_id(dir, &identity);
    ClaudeAccount::new(
        id,
        None,
        identity.email,
        identity.org_id,
        identity.org_name,
        identity.subscription_type,
        dir.to_path_buf(),
        source,
        utc_now(),
        utc_now(),
        None,
    )
}

fn build_discovered_account(
    matched: Option<&ClaudeAccount>,
    identity: ClaudeIdentity,
    dir: PathBuf,
    source: ClaudeAccountSource,
    discovered_at: chrono::DateTime<chrono::Utc>,
) -> ClaudeAccount {
    let id = matched
        .map(|account| account.id)
        .unwrap_or_else(|| stable_discovered_id(&dir, &identity));
    ClaudeAccount::new(
        id,
        matched.and_then(|account| account.nickname.clone()),
        identity
            .email
            .or_else(|| matched.and_then(|account| account.email_hint.clone())),
        identity
            .org_id
            .or_else(|| matched.and_then(|account| account.org_id.clone())),
        identity
            .org_name
            .or_else(|| matched.and_then(|account| account.org_name.clone())),
        identity
            .subscription_type
            .or_else(|| matched.and_then(|account| account.subscription_type.clone())),
        dir,
        source,
        matched
            .map(|account| account.created_at)
            .unwrap_or(discovered_at),
        matched
            .map(|account| account.updated_at.max(discovered_at))
            .unwrap_or(discovered_at),
        matched
            .and_then(|account| account.last_authenticated_at)
            .or(Some(discovered_at)),
    )
}

fn read_ambient_oauth_account() -> Option<AmbientOauthAccount> {
    let path = ambient_claude_json_path();
    let content = fs::read_to_string(path).ok()?;
    let root: serde_json::Value = serde_json::from_str(&content).ok()?;
    let oauth = root.get("oauthAccount")?.as_object()?;
    let email_address = oauth
        .get("emailAddress")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let organization_uuid = oauth
        .get("organizationUuid")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    if email_address.is_none() && organization_uuid.is_none() {
        return None;
    }
    Some(AmbientOauthAccount {
        email_address,
        organization_uuid,
    })
}

fn read_claude_ai_oauth_str_field(dir: &Path, field: &str) -> Option<String> {
    let root = credentials_merge::read_root(dir).ok()?;
    root.get("claudeAiOauth")?
        .get(field)?
        .as_str()
        .map(str::to_string)
}

fn directory_timestamp(path: &Path) -> chrono::DateTime<chrono::Utc> {
    let creds_path = credentials_merge::credentials_file_path(path);
    if creds_path.exists()
        && let Ok(metadata) = fs::metadata(&creds_path)
        && let Ok(modified) = metadata.modified()
    {
        return modified.into();
    }
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map(Into::into)
        .unwrap_or_else(|_| utc_now())
}

fn standardized(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_lowercase()
}

fn timestamp_slug() -> String {
    utc_now().format("%Y%m%d-%H%M%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_accounts::file_locations::{
        clear_ambient_claude_config_dir_override, clear_ambient_claude_json_path_override,
        clear_app_support_directory_override, with_ambient_claude_config_dir,
        with_ambient_claude_json_path, with_app_support_directory,
    };

    fn write_credentials(dir: &Path, access_token: &str, subscription_type: &str) {
        fs::create_dir_all(dir).unwrap();
        let payload = serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access_token,
                "refreshToken": format!("refresh-{access_token}"),
                "subscriptionType": subscription_type,
            },
            "mcpOAuth": {"some-server": {"accessToken": "keepme"}},
        });
        fs::write(
            credentials_merge::credentials_file_path(dir),
            serde_json::to_vec_pretty(&payload).unwrap(),
        )
        .unwrap();
    }

    fn write_ambient_claude_json(path: &Path, email: &str, org_uuid: &str) {
        let payload = serde_json::json!({
            "oauthAccount": {
                "emailAddress": email,
                "organizationUuid": org_uuid,
            }
        });
        fs::write(path, serde_json::to_vec_pretty(&payload).unwrap()).unwrap();
    }

    fn make_account(dir: PathBuf, email: &str, org_uuid: &str) -> ClaudeAccount {
        ClaudeAccount::new(
            Uuid::new_v4(),
            None,
            Some(email.to_string()),
            Some(org_uuid.to_string()),
            None,
            None,
            dir,
            ClaudeAccountSource::ManagedByApp,
            utc_now(),
            utc_now(),
            Some(utc_now()),
        )
    }

    // ── Threat Matrix: Destructive file writes ──────────────────────────

    #[test]
    fn remove_refuses_a_path_outside_the_managed_configs_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        with_app_support_directory(root.to_path_buf());

        // `outside` lives beside (not under) `managed-configs/`.
        let outside = root.join("outside-not-managed");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("marker.txt"), "do not delete me").unwrap();

        let account = make_account(outside.clone(), "user@example.com", "org-1");
        let manager = ClaudeAccountManager::new();
        let result = manager.remove_managed_files_if_owned(&account);

        assert!(
            result.is_err(),
            "must refuse to remove a path outside managed-configs/"
        );
        assert!(
            outside.exists(),
            "the outside directory must survive the refused removal"
        );
        assert!(outside.join("marker.txt").exists());

        clear_app_support_directory_override();
    }

    #[test]
    fn remove_refuses_the_managed_configs_root_itself() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        with_app_support_directory(root.to_path_buf());
        ensure_directories().unwrap();

        let account = make_account(managed_configs_directory(), "user@example.com", "org-1");
        let manager = ClaudeAccountManager::new();
        let result = manager.remove_managed_files_if_owned(&account);

        assert!(
            result.is_err(),
            "must refuse to remove the managed-configs root"
        );
        assert!(managed_configs_directory().exists());

        clear_app_support_directory_override();
    }

    #[test]
    fn remove_deletes_a_genuinely_owned_managed_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        with_app_support_directory(root.to_path_buf());

        let owned = managed_configs_directory().join("11111111-1111-1111-1111-111111111111");
        write_credentials(&owned, "token", "pro");

        let account = make_account(owned.clone(), "user@example.com", "org-1");
        let manager = ClaudeAccountManager::new();
        manager.remove_managed_files_if_owned(&account).unwrap();

        assert!(!owned.exists());

        clear_app_support_directory_override();
    }

    #[test]
    fn remove_is_a_no_op_for_ambient_source() {
        let dir = tempfile::tempdir().unwrap();
        with_app_support_directory(dir.path().to_path_buf());

        let ambient_dir = dir.path().join("ambient");
        write_credentials(&ambient_dir, "token", "pro");
        let mut account = make_account(ambient_dir.clone(), "user@example.com", "org-1");
        account.source = ClaudeAccountSource::Ambient;

        let manager = ClaudeAccountManager::new();
        manager.remove_managed_files_if_owned(&account).unwrap();
        assert!(
            ambient_dir.exists(),
            "ambient accounts are never file-owned"
        );

        clear_app_support_directory_override();
    }

    // ── discover_ambient_account / active identity ──────────────────────

    #[test]
    fn discover_ambient_account_reads_oauth_account_and_subscription_type() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        with_app_support_directory(root.to_path_buf());
        let ambient_dir = root.join(".claude");
        with_ambient_claude_config_dir(ambient_dir.clone());
        let claude_json = root.join(".claude.json");
        with_ambient_claude_json_path(claude_json.clone());

        write_credentials(&ambient_dir, "ambient-token", "max");
        write_ambient_claude_json(&claude_json, "ambient@example.com", "org-ambient");

        let manager = ClaudeAccountManager::new();
        let account = manager
            .discover_ambient_account(&[])
            .expect("ambient account discovered");
        assert_eq!(account.email_hint.as_deref(), Some("ambient@example.com"));
        assert_eq!(account.org_id.as_deref(), Some("org-ambient"));
        assert_eq!(account.subscription_type.as_deref(), Some("max"));
        assert_eq!(account.source, ClaudeAccountSource::Ambient);

        clear_app_support_directory_override();
        clear_ambient_claude_config_dir_override();
        clear_ambient_claude_json_path_override();
    }

    #[test]
    fn discover_ambient_account_none_without_credentials_file() {
        let dir = tempfile::tempdir().unwrap();
        with_app_support_directory(dir.path().to_path_buf());
        with_ambient_claude_config_dir(dir.path().join("no-credentials-here"));

        let manager = ClaudeAccountManager::new();
        assert!(manager.discover_ambient_account(&[]).is_none());

        clear_app_support_directory_override();
        clear_ambient_claude_config_dir_override();
    }

    #[test]
    fn load_active_identity_reads_oauth_account_and_access_token() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        with_app_support_directory(root.to_path_buf());
        let ambient_dir = root.join(".claude");
        with_ambient_claude_config_dir(ambient_dir.clone());
        let claude_json = root.join(".claude.json");
        with_ambient_claude_json_path(claude_json.clone());

        write_credentials(&ambient_dir, "ambient-token", "max");
        write_ambient_claude_json(&claude_json, "ambient@example.com", "org-ambient");

        let manager = ClaudeAccountManager::new();
        let identity = manager.load_active_identity();
        assert_eq!(
            identity.oauth_account.unwrap().email_address.as_deref(),
            Some("ambient@example.com")
        );
        assert_eq!(identity.access_token.as_deref(), Some("ambient-token"));

        clear_app_support_directory_override();
        clear_ambient_claude_config_dir_override();
        clear_ambient_claude_json_path_override();
    }

    // ── switch_active_account ────────────────────────────────────────────

    #[test]
    fn switch_merges_claude_ai_oauth_and_preserves_mcp_oauth_and_backs_up() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        with_app_support_directory(root.to_path_buf());
        let ambient_dir = root.join(".claude");
        with_ambient_claude_config_dir(ambient_dir.clone());

        write_credentials(&ambient_dir, "old-token", "pro");
        let target_dir = managed_configs_directory().join("target");
        write_credentials(&target_dir, "new-token", "max");

        let target_account = make_account(target_dir.clone(), "new@example.com", "org-new");
        let manager = ClaudeAccountManager::new();
        let result = manager
            .switch_active_account(&target_account, std::slice::from_ref(&target_account))
            .unwrap();

        let ambient_root = credentials_merge::read_root(&ambient_dir).unwrap();
        assert_eq!(ambient_root["claudeAiOauth"]["accessToken"], "new-token");
        assert_eq!(
            ambient_root["mcpOAuth"]["some-server"]["accessToken"],
            "keepme"
        );

        let backup_path = result.backup_path.expect("backup created");
        let backup_root: serde_json::Value =
            serde_json::from_slice(&fs::read(&backup_path).unwrap()).unwrap();
        assert_eq!(backup_root["claudeAiOauth"]["accessToken"], "old-token");

        clear_app_support_directory_override();
        clear_ambient_claude_config_dir_override();
    }

    #[test]
    fn switch_rejects_target_without_credentials_file() {
        let dir = tempfile::tempdir().unwrap();
        with_app_support_directory(dir.path().to_path_buf());
        with_ambient_claude_config_dir(dir.path().join(".claude"));

        let missing_dir = managed_configs_directory().join("missing");
        let target_account = make_account(missing_dir, "new@example.com", "org-new");
        let manager = ClaudeAccountManager::new();
        let result = manager.switch_active_account(&target_account, &[]);
        assert!(result.is_err());

        clear_app_support_directory_override();
        clear_ambient_claude_config_dir_override();
    }
}
