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
    AmbientClaudeIdentity, AmbientOauthAccount, ClaudeIdentity, claude_json_path,
    load_identity_from_files, load_identity_from_path, parse_auth_status_json,
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

        // Identity comes from ambient `~/.claude.json` `oauthAccount` via the
        // shared parser, so `org_name` is populated (it was hardcoded `None`
        // here before #12). `subscription_type` still comes from
        // `.credentials.json`, which `oauthAccount` does not carry.
        let mut identity = load_identity_from_path(&ambient_claude_json_path());
        identity.subscription_type = read_claude_ai_oauth_str_field(&dir, "subscriptionType");
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

        // Read the target's `oauthAccount` BEFORE any write: a read here cannot
        // leave a partial state, so failing early is free.
        let target_oauth = target_oauth_account_value(target);

        let mut materialized_account: Option<ClaudeAccount> = None;
        if let Some(ambient) = &ambient_account {
            let is_ambient = ambient.source == ClaudeAccountSource::Ambient;
            if is_ambient && !ambient.matches(target) {
                materialized_account = Some(self.materialize_as_managed(ambient, existing)?);
            }
        }

        fs::create_dir_all(&ambient_dir)?;
        // Captured BEFORE both writes below so either one can be rolled back.
        let backup_path = self.backup_ambient_credentials(&ambient_dir)?;

        let mut ambient_root = if credentials_merge::credentials_file_path(&ambient_dir).exists() {
            credentials_merge::read_root(&ambient_dir)?
        } else {
            serde_json::json!({})
        };
        let source_root = credentials_merge::read_root(&target.claude_config_dir)?;
        credentials_merge::merge_claude_ai_oauth(&mut ambient_root, &source_root)?;
        credentials_merge::write_root(&ambient_dir, &ambient_root)?;

        // Second write: key-merge the target's `oauthAccount` into ambient
        // `~/.claude.json` so file-based active detection reflects the switch
        // immediately. On failure, roll the `.credentials.json` write back and
        // return an error — a partial switch (credentials say B, identity says
        // A, badge wrong with no visible cause) is worse than no switch.
        if let Some(oauth) = target_oauth {
            let claude_json = ambient_claude_json_path();
            let write = (|| -> Result<(), CredentialsMergeError> {
                let mut root = if claude_json.exists() {
                    credentials_merge::read_json_root(&claude_json)?
                } else {
                    serde_json::json!({})
                };
                credentials_merge::merge_top_level_key(&mut root, "oauthAccount", oauth)?;
                credentials_merge::write_json_root_atomic(&claude_json, &root)
            })();
            if let Err(error) = write {
                self.restore_ambient_credentials(&ambient_dir, backup_path.as_deref());
                return Err(ClaudeAccountManagerError::Message(format!(
                    "Switched credentials were rolled back: the active identity file \
                     could not be updated ({error})."
                )));
            }
        } else {
            tracing::warn!(
                dir = %target.claude_config_dir.display(),
                "switch target has no resolvable oauthAccount; ambient ~/.claude.json \
                 identity left unchanged"
            );
        }

        Ok(ClaudeSwitchResult {
            materialized_account,
            backup_path,
            ambient_account: self.discover_ambient_account(existing),
        })
    }

    /// Restore ambient `.credentials.json` after a failed switch. `backup` is
    /// `None` when the ambient directory had no credentials file before this
    /// switch, in which case the just-written file is removed.
    fn restore_ambient_credentials(&self, ambient_dir: &Path, backup: Option<&Path>) {
        let creds_path = credentials_merge::credentials_file_path(ambient_dir);
        match backup {
            Some(backup) => {
                let _ = fs::copy(backup, &creds_path);
            }
            None => {
                let _ = fs::remove_file(&creds_path);
            }
        }
    }

    /// Materialize `account` as an app-managed config directory.
    ///
    /// Dedupes FIRST: when `account` already `matches()` a stored
    /// `ManagedByApp` account whose directory still has `.credentials.json`,
    /// that directory's `claudeAiOauth` + `oauthAccount` are refreshed from
    /// `account` and the existing account is returned — no new directory is
    /// created (bug D: a switch used to spawn a fresh dir every time). On a
    /// genuine miss it creates a directory containing both `.credentials.json`
    /// (copied whole, preserving `mcpOAuth`) and a `.claude.json` carrying the
    /// source's `oauthAccount` so later discovery is self-describing.
    pub fn materialize_as_managed(
        &self,
        account: &ClaudeAccount,
        existing: &[ClaudeAccount],
    ) -> Result<ClaudeAccount, ClaudeAccountManagerError> {
        ensure_directories()?;

        let source_path = credentials_merge::credentials_file_path(&account.claude_config_dir);
        if !source_path.exists() {
            return Err(ClaudeAccountManagerError::Message(
                "The current active account does not contain `.credentials.json`.".to_string(),
            ));
        }

        if let Some(hit) = existing.iter().find(|candidate| {
            candidate.source == ClaudeAccountSource::ManagedByApp
                && candidate.matches(account)
                && credentials_merge::credentials_file_path(&candidate.claude_config_dir).exists()
        }) {
            self.refresh_managed_dir_from(account, hit)?;
            return Ok(hit.clone());
        }

        let destination_dir = managed_configs_directory().join(Uuid::new_v4().to_string());
        fs::create_dir_all(&destination_dir)?;
        fs::copy(
            &source_path,
            credentials_merge::credentials_file_path(&destination_dir),
        )?;
        if let Some(oauth) = source_oauth_account_value(account) {
            let mut root = serde_json::json!({});
            credentials_merge::merge_top_level_key(&mut root, "oauthAccount", oauth)?;
            credentials_merge::write_json_root_atomic(&claude_json_path(&destination_dir), &root)?;
        }

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

    /// On a dedupe hit, key-merge the source account's fresh `claudeAiOauth`
    /// and `oauthAccount` into the existing managed directory so switching back
    /// to it later does not restore stale tokens (design F6). Key-scoped, so
    /// the hit directory's `mcpOAuth` and other siblings survive.
    fn refresh_managed_dir_from(
        &self,
        source: &ClaudeAccount,
        hit: &ClaudeAccount,
    ) -> Result<(), ClaudeAccountManagerError> {
        let source_root = credentials_merge::read_root(&source.claude_config_dir)?;
        let mut hit_root = credentials_merge::read_root(&hit.claude_config_dir)?;
        credentials_merge::merge_claude_ai_oauth(&mut hit_root, &source_root)?;
        credentials_merge::write_root(&hit.claude_config_dir, &hit_root)?;

        if let Some(oauth) = source_oauth_account_value(source) {
            let path = claude_json_path(&hit.claude_config_dir);
            let mut root = if path.exists() {
                credentials_merge::read_json_root(&path)?
            } else {
                serde_json::json!({})
            };
            credentials_merge::merge_top_level_key(&mut root, "oauthAccount", oauth)?;
            credentials_merge::write_json_root_atomic(&path, &root)?;
        }
        Ok(())
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

        // Identity comes from the just-provisioned directory's own
        // `.claude.json` (bug B: this used to read ambient identity). A short
        // bounded retry absorbs filesystem flush latency after `claude auth
        // login` writes the file. `claude auth status --json` — already scoped
        // to `dir` via `CLAUDE_CONFIG_DIR` — is kept only as a fallback and may
        // additionally carry `subscriptionType`.
        let identity = wait_for_managed_identity(dir)
            .or_else(|| {
                self.capture_identity_via_cli(dir)
                    .ok()
                    .filter(|captured| captured.email.is_some() || captured.org_id.is_some())
            })
            .unwrap_or_default();
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

        // The directory's own `.claude.json` `oauthAccount` is PRIMARY for
        // email/org/org_name (bug A: this used to read only the stored record,
        // so an unmatched dir rendered as a shrunk UUID). The stored record is
        // a fallback and still supplies nickname + timestamps via
        // `build_discovered_account`.
        let file_identity = load_identity_from_files(dir);
        let subscription_type = read_claude_ai_oauth_str_field(dir, "subscriptionType")
            .or_else(|| existing_for_dir.and_then(|account| account.subscription_type.clone()));
        let identity = ClaudeIdentity {
            email: file_identity
                .email
                .or_else(|| existing_for_dir.and_then(|account| account.email_hint.clone())),
            org_id: file_identity
                .org_id
                .or_else(|| existing_for_dir.and_then(|account| account.org_id.clone())),
            org_name: file_identity
                .org_name
                .or_else(|| existing_for_dir.and_then(|account| account.org_name.clone())),
            subscription_type,
        };
        // Orphan filter (design F8) — mirrors `discover_ambient_account`. A
        // `.credentials.json`-only directory with no resolvable identity is
        // hidden from the listing; the files are left on disk untouched.
        if identity.email.is_none() && identity.org_id.is_none() {
            return None;
        }

        let discovered_at = directory_timestamp(dir);
        let candidate = candidate_account(identity.clone(), dir, ClaudeAccountSource::ManagedByApp);
        // ID-CHURN FIX: `existing_for_dir` (same directory) is a strictly
        // stronger signal than an identity `matches()` scan, and with the
        // loosened `matches()` the scan could otherwise adopt a *different*
        // account's id for this directory. Try the same-dir record first.
        let matched =
            existing_for_dir.or_else(|| existing.iter().find(|account| candidate.matches(account)));
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
    let identity = load_identity_from_path(&ambient_claude_json_path());
    if identity.email.is_none() && identity.org_id.is_none() {
        return None;
    }
    Some(AmbientOauthAccount {
        email_address: identity.email,
        organization_uuid: identity.org_id,
    })
}

/// Poll `<dir>/.claude.json` for up to ~1 s (5 × 200 ms) so a freshly written
/// identity file is picked up despite filesystem flush latency after `claude
/// auth login`. 1 s against the 180 s login timeout is free. Never falls back
/// to ambient — that path is unreachable from the add flow.
fn wait_for_managed_identity(dir: &Path) -> Option<ClaudeIdentity> {
    for attempt in 0..5 {
        let identity = load_identity_from_files(dir);
        if identity.email.is_some() || identity.org_id.is_some() {
            return Some(identity);
        }
        if attempt < 4 {
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    None
}

/// The raw `oauthAccount` JSON object from a `.claude.json` file, verbatim
/// (so fields we do not model, e.g. `accountUuid`, are preserved on a switch).
fn read_oauth_account_object(claude_json: &Path) -> Option<serde_json::Value> {
    let content = fs::read_to_string(claude_json).ok()?;
    let root: serde_json::Value = serde_json::from_str(&content).ok()?;
    let oauth = root.get("oauthAccount")?;
    oauth.is_object().then(|| oauth.clone())
}

/// Synthesize a minimal `oauthAccount` object from a stored account's fields.
/// Only reached when the source `.claude.json` has no usable `oauthAccount`.
fn synthesize_oauth_account_object(account: &ClaudeAccount) -> Option<serde_json::Value> {
    if account.email_hint.is_none() && account.org_id.is_none() {
        return None;
    }
    let mut obj = serde_json::Map::new();
    if let Some(email) = &account.email_hint {
        obj.insert(
            "emailAddress".to_string(),
            serde_json::Value::String(email.clone()),
        );
    }
    if let Some(org_id) = &account.org_id {
        obj.insert(
            "organizationUuid".to_string(),
            serde_json::Value::String(org_id.clone()),
        );
    }
    if let Some(org_name) = &account.org_name {
        obj.insert(
            "organizationName".to_string(),
            serde_json::Value::String(org_name.clone()),
        );
    }
    Some(serde_json::Value::Object(obj))
}

/// `oauthAccount` value to write into ambient `~/.claude.json` when switching
/// to `target`: the target directory's real object if present, else a
/// synthesized 3-field object (with a warning).
fn target_oauth_account_value(target: &ClaudeAccount) -> Option<serde_json::Value> {
    read_oauth_account_object(&claude_json_path(&target.claude_config_dir)).or_else(|| {
        tracing::warn!(
            dir = %target.claude_config_dir.display(),
            "target managed dir has no oauthAccount in .claude.json; synthesizing from the stored record"
        );
        synthesize_oauth_account_object(target)
    })
}

/// `oauthAccount` value for a materialization source, resolved by source kind:
/// `Ambient` → `~/.claude.json`; `ManagedByApp` → `<dir>/.claude.json`.
fn source_oauth_account_value(account: &ClaudeAccount) -> Option<serde_json::Value> {
    let claude_json = match account.source {
        ClaudeAccountSource::Ambient => ambient_claude_json_path(),
        ClaudeAccountSource::ManagedByApp => claude_json_path(&account.claude_config_dir),
    };
    read_oauth_account_object(&claude_json).or_else(|| synthesize_oauth_account_object(account))
}

/// Pure. Rebuild the stored account listing so it reflects on-disk reality:
///
/// - drop `ManagedByApp` rows whose directory lives under `managed_root` but no
///   longer survives discovery (the orphan filter, propagated to the store);
/// - keep every row whose directory is outside `managed_root`;
/// - hydrate stale/empty stored identities from the fresh discovered record.
///
/// Never deletes anything on disk. Never re-keys ids: managed directories are
/// uuid-named, so `stable_discovered_id` returns the directory uuid verbatim
/// regardless of identity, and hydrating email/org cannot change the id.
pub fn reconcile_stored_accounts(
    existing: &[ClaudeAccount],
    managed: &[ClaudeAccount],
    managed_root: &Path,
) -> Vec<ClaudeAccount> {
    let root = standardized(managed_root);
    existing
        .iter()
        .filter(|account| {
            account.source != ClaudeAccountSource::ManagedByApp
                || !account.standardized_config_dir().starts_with(&root)
                || managed.iter().any(|fresh| {
                    fresh.standardized_config_dir() == account.standardized_config_dir()
                })
        })
        .map(|account| {
            let mut account = account.clone();
            if let Some(fresh) = managed.iter().find(|fresh| fresh.matches(&account)) {
                account.merge_from(fresh);
            }
            account
        })
        .collect()
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

    /// Ambient `.claude.json` with an unrelated sibling key, so a switch can be
    /// checked for leaving non-`oauthAccount` state byte-identical.
    fn write_ambient_claude_json_with_sibling(path: &Path, email: &str, org_uuid: &str) {
        let payload = serde_json::json!({
            "oauthAccount": {
                "emailAddress": email,
                "organizationUuid": org_uuid,
            },
            "projects": {"/some/path": {"lastOpened": "2026-01-01"}},
            "numStartups": 42,
        });
        fs::write(path, serde_json::to_vec_pretty(&payload).unwrap()).unwrap();
    }

    /// A managed dir's own `<dir>/.claude.json`.
    fn write_managed_claude_json(dir: &Path, email: &str, org_uuid: &str) {
        fs::create_dir_all(dir).unwrap();
        let payload = serde_json::json!({
            "oauthAccount": {
                "emailAddress": email,
                "organizationUuid": org_uuid,
                "organizationName": format!("{org_uuid}-name"),
                "accountUuid": format!("acct-{email}"),
            }
        });
        fs::write(
            claude_json_path(dir),
            serde_json::to_vec_pretty(&payload).unwrap(),
        )
        .unwrap();
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
        let ambient_claude_json = root.join(".claude.json");
        with_ambient_claude_json_path(ambient_claude_json.clone());

        write_credentials(&ambient_dir, "old-token", "pro");
        write_ambient_claude_json(&ambient_claude_json, "old@example.com", "org-old");
        let target_dir = managed_configs_directory().join("target");
        write_credentials(&target_dir, "new-token", "max");
        write_managed_claude_json(&target_dir, "new@example.com", "org-new");

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

        // The ambient active identity now tracks the switch target.
        let ambient_json: serde_json::Value =
            serde_json::from_slice(&fs::read(&ambient_claude_json).unwrap()).unwrap();
        assert_eq!(
            ambient_json["oauthAccount"]["emailAddress"],
            "new@example.com"
        );
        assert_eq!(ambient_json["oauthAccount"]["organizationUuid"], "org-new");

        let backup_path = result.backup_path.expect("backup created");
        let backup_root: serde_json::Value =
            serde_json::from_slice(&fs::read(&backup_path).unwrap()).unwrap();
        assert_eq!(backup_root["claudeAiOauth"]["accessToken"], "old-token");

        clear_app_support_directory_override();
        clear_ambient_claude_config_dir_override();
        clear_ambient_claude_json_path_override();
    }

    #[test]
    fn switch_rejects_target_without_credentials_file() {
        let dir = tempfile::tempdir().unwrap();
        with_app_support_directory(dir.path().to_path_buf());
        with_ambient_claude_config_dir(dir.path().join(".claude"));
        with_ambient_claude_json_path(dir.path().join(".claude.json"));

        let missing_dir = managed_configs_directory().join("missing");
        let target_account = make_account(missing_dir, "new@example.com", "org-new");
        let manager = ClaudeAccountManager::new();
        let result = manager.switch_active_account(&target_account, &[]);
        assert!(result.is_err());

        clear_app_support_directory_override();
        clear_ambient_claude_config_dir_override();
        clear_ambient_claude_json_path_override();
    }

    // ── per-directory identity (#12) ────────────────────────────────────

    // [R2] a managed dir with its own `.claude.json` is listed with the email
    // it carries, keyed by the (uuid) directory name — not a shrunk UUID.
    #[test]
    fn managed_dir_with_claude_json_lists_with_email_not_uuid() {
        let dir = tempfile::tempdir().unwrap();
        with_app_support_directory(dir.path().to_path_buf());
        ensure_directories().unwrap();
        let uuid = "f47ac10b-58cc-4372-a567-0e02b2c3d479";
        let managed = managed_configs_directory().join(uuid);
        write_credentials(&managed, "tok", "pro");
        write_managed_claude_json(&managed, "listed@x.com", "org-listed");

        let discovered = ClaudeAccountManager::new()
            .discover_managed_accounts(&[])
            .unwrap();
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].email_hint.as_deref(), Some("listed@x.com"));
        assert_eq!(discovered[0].org_id.as_deref(), Some("org-listed"));
        assert_eq!(discovered[0].id, Uuid::parse_str(uuid).unwrap());

        clear_app_support_directory_override();
    }

    // [R3] file identity overrides a *wrong* stored email, while the stored
    // nickname and `created_at` are carried forward.
    #[test]
    fn file_identity_beats_wrong_stored_email_but_keeps_nickname_and_created_at() {
        let dir = tempfile::tempdir().unwrap();
        with_app_support_directory(dir.path().to_path_buf());
        ensure_directories().unwrap();
        let uuid = "f47ac10b-58cc-4372-a567-0e02b2c3d479";
        let managed = managed_configs_directory().join(uuid);
        write_credentials(&managed, "tok", "pro");
        write_managed_claude_json(&managed, "correct@x.com", "org-correct");

        let created = utc_now() - chrono::Duration::days(3);
        let mut stored = make_account(managed.clone(), "wrong@x.com", "org-wrong");
        stored.id = Uuid::parse_str(uuid).unwrap();
        stored.nickname = Some("Work".to_string());
        stored.created_at = created;

        let discovered = ClaudeAccountManager::new()
            .discover_managed_accounts(std::slice::from_ref(&stored))
            .unwrap();
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].email_hint.as_deref(), Some("correct@x.com"));
        assert_eq!(discovered[0].org_id.as_deref(), Some("org-correct"));
        assert_eq!(discovered[0].nickname.as_deref(), Some("Work"));
        assert_eq!(discovered[0].created_at, created);

        clear_app_support_directory_override();
    }

    // [R4] a `.credentials.json`-only dir with no resolvable identity is hidden
    // from the listing but left untouched on disk.
    #[test]
    fn credentials_only_dir_is_hidden_but_left_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        with_app_support_directory(dir.path().to_path_buf());
        ensure_directories().unwrap();
        let managed = managed_configs_directory().join("f47ac10b-58cc-4372-a567-0e02b2c3d479");
        write_credentials(&managed, "tok", "pro");

        let discovered = ClaudeAccountManager::new()
            .discover_managed_accounts(&[])
            .unwrap();
        assert!(discovered.is_empty());
        assert!(credentials_merge::credentials_file_path(&managed).exists());

        clear_app_support_directory_override();
    }

    // [R5] `wait_for_managed_identity` resolves the dir's own `.claude.json`
    // when present and returns `None` after a bounded retry when it never
    // appears. It spawns no subprocess (it only reads a file).
    #[test]
    fn wait_for_managed_identity_hits_on_file_and_bounds_the_retry_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        assert!(wait_for_managed_identity(dir.path()).is_none());
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "retry must stay bounded (~1s)"
        );

        write_managed_claude_json(dir.path(), "add@x.com", "org-add");
        let hit = wait_for_managed_identity(dir.path()).expect("identity present");
        assert_eq!(hit.email.as_deref(), Some("add@x.com"));
        assert_eq!(hit.org_id.as_deref(), Some("org-add"));
    }

    // [R6] after a switch, ambient `~/.claude.json` `oauthAccount` equals the
    // target's verbatim object, unrelated sibling keys are byte-identical, and
    // the target dir's own `.claude.json` is unchanged.
    #[test]
    fn switch_updates_ambient_oauth_account_and_leaves_siblings_and_target_intact() {
        let dir = tempfile::tempdir().unwrap();
        let rootp = dir.path();
        with_app_support_directory(rootp.to_path_buf());
        let ambient_dir = rootp.join(".claude");
        with_ambient_claude_config_dir(ambient_dir.clone());
        let ambient_json = rootp.join(".claude.json");
        with_ambient_claude_json_path(ambient_json.clone());

        write_credentials(&ambient_dir, "old-token", "pro");
        write_ambient_claude_json_with_sibling(&ambient_json, "old@x.com", "org-old");
        let sibling_before: serde_json::Value =
            serde_json::from_slice(&fs::read(&ambient_json).unwrap()).unwrap();

        let target_dir = managed_configs_directory().join("target");
        write_credentials(&target_dir, "new-token", "max");
        write_managed_claude_json(&target_dir, "new@x.com", "org-new");
        let target_json_before = fs::read(claude_json_path(&target_dir)).unwrap();

        let target_account = make_account(target_dir.clone(), "new@x.com", "org-new");
        ClaudeAccountManager::new()
            .switch_active_account(&target_account, std::slice::from_ref(&target_account))
            .unwrap();

        let after: serde_json::Value =
            serde_json::from_slice(&fs::read(&ambient_json).unwrap()).unwrap();
        assert_eq!(after["oauthAccount"]["emailAddress"], "new@x.com");
        assert_eq!(after["oauthAccount"]["organizationUuid"], "org-new");
        assert_eq!(after["oauthAccount"]["accountUuid"], "acct-new@x.com");
        assert_eq!(after["projects"], sibling_before["projects"]);
        assert_eq!(after["numStartups"], sibling_before["numStartups"]);
        assert_eq!(
            fs::read(claude_json_path(&target_dir)).unwrap(),
            target_json_before
        );

        clear_app_support_directory_override();
        clear_ambient_claude_config_dir_override();
        clear_ambient_claude_json_path_override();
    }

    // [R7] if the ambient `~/.claude.json` write fails (pre-existing malformed
    // file), the `.credentials.json` merge is rolled back and the switch errors.
    #[test]
    fn switch_rolls_back_credentials_when_ambient_claude_json_is_malformed() {
        let dir = tempfile::tempdir().unwrap();
        let rootp = dir.path();
        with_app_support_directory(rootp.to_path_buf());
        let ambient_dir = rootp.join(".claude");
        with_ambient_claude_config_dir(ambient_dir.clone());
        let ambient_json = rootp.join(".claude.json");
        with_ambient_claude_json_path(ambient_json.clone());

        write_credentials(&ambient_dir, "old-token", "pro");
        fs::write(&ambient_json, "{ this is not valid json").unwrap();

        let target_dir = managed_configs_directory().join("target");
        write_credentials(&target_dir, "new-token", "max");
        write_managed_claude_json(&target_dir, "new@x.com", "org-new");

        let target_account = make_account(target_dir.clone(), "new@x.com", "org-new");
        let result = ClaudeAccountManager::new()
            .switch_active_account(&target_account, std::slice::from_ref(&target_account));
        assert!(result.is_err(), "malformed ambient .claude.json must fail");

        let restored = credentials_merge::read_root(&ambient_dir).unwrap();
        assert_eq!(restored["claudeAiOauth"]["accessToken"], "old-token");
        assert_eq!(restored["mcpOAuth"]["some-server"]["accessToken"], "keepme");

        clear_app_support_directory_override();
        clear_ambient_claude_config_dir_override();
        clear_ambient_claude_json_path_override();
    }

    // [R8] a dedupe hit refreshes the existing managed dir's tokens and returns
    // it without creating a new directory.
    #[test]
    fn materialize_dedupes_without_creating_a_dir_and_refreshes_tokens() {
        let dir = tempfile::tempdir().unwrap();
        with_app_support_directory(dir.path().to_path_buf());
        ensure_directories().unwrap();
        let ambient_json = dir.path().join(".claude.json");
        with_ambient_claude_json_path(ambient_json.clone());
        write_ambient_claude_json(&ambient_json, "b@x.com", "org-b");

        let managed_b = managed_configs_directory().join("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
        write_credentials(&managed_b, "stale-token", "pro");
        write_managed_claude_json(&managed_b, "b@x.com", "org-b");
        let stored_b = make_account(managed_b.clone(), "b@x.com", "org-b");

        let ambient_dir = dir.path().join("ambient");
        write_credentials(&ambient_dir, "fresh-token", "max");
        let mut ambient_acct = make_account(ambient_dir.clone(), "b@x.com", "org-b");
        ambient_acct.source = ClaudeAccountSource::Ambient;

        let before = fs::read_dir(managed_configs_directory()).unwrap().count();
        let out = ClaudeAccountManager::new()
            .materialize_as_managed(&ambient_acct, std::slice::from_ref(&stored_b))
            .unwrap();
        let after = fs::read_dir(managed_configs_directory()).unwrap().count();

        assert_eq!(before, after, "no new managed dir on a dedupe hit");
        assert_eq!(out.claude_config_dir, managed_b);
        let refreshed = credentials_merge::read_root(&managed_b).unwrap();
        assert_eq!(refreshed["claudeAiOauth"]["accessToken"], "fresh-token");
        assert_eq!(
            refreshed["mcpOAuth"]["some-server"]["accessToken"],
            "keepme"
        );

        clear_app_support_directory_override();
        clear_ambient_claude_json_path_override();
    }

    // [R8] a genuine miss creates a dir carrying BOTH `.credentials.json` and a
    // `.claude.json` with `oauthAccount`.
    #[test]
    fn materialize_miss_creates_dir_with_both_credentials_and_claude_json() {
        let dir = tempfile::tempdir().unwrap();
        with_app_support_directory(dir.path().to_path_buf());
        ensure_directories().unwrap();
        let ambient_json = dir.path().join(".claude.json");
        with_ambient_claude_json_path(ambient_json.clone());
        write_ambient_claude_json(&ambient_json, "amb@x.com", "org-amb");

        let ambient_dir = dir.path().join("ambient");
        write_credentials(&ambient_dir, "amb-token", "max");
        let mut ambient_acct = make_account(ambient_dir.clone(), "amb@x.com", "org-amb");
        ambient_acct.source = ClaudeAccountSource::Ambient;

        let before = fs::read_dir(managed_configs_directory()).unwrap().count();
        let out = ClaudeAccountManager::new()
            .materialize_as_managed(&ambient_acct, &[])
            .unwrap();
        let after = fs::read_dir(managed_configs_directory()).unwrap().count();
        assert_eq!(after, before + 1);
        assert!(credentials_merge::credentials_file_path(&out.claude_config_dir).exists());
        let claude_json: serde_json::Value =
            serde_json::from_slice(&fs::read(claude_json_path(&out.claude_config_dir)).unwrap())
                .unwrap();
        assert_eq!(claude_json["oauthAccount"]["emailAddress"], "amb@x.com");
        assert_eq!(claude_json["oauthAccount"]["organizationUuid"], "org-amb");

        clear_app_support_directory_override();
        clear_ambient_claude_json_path_override();
    }

    // [R10] reconciliation drops an orphan row under the managed root, keeps a
    // row pointing outside it, and hydrates a stale stored email from files.
    #[test]
    fn reconcile_drops_orphan_under_root_keeps_outside_and_hydrates_stale_email() {
        let dir = tempfile::tempdir().unwrap();
        with_app_support_directory(dir.path().to_path_buf());
        ensure_directories().unwrap();
        let root = managed_configs_directory();

        let live_dir = root.join("11111111-1111-1111-1111-111111111111");
        let mut live_stored = make_account(live_dir.clone(), "stale@x.com", "org-live");
        live_stored.id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        let mut live_fresh = make_account(live_dir.clone(), "fresh@x.com", "org-live");
        live_fresh.id = live_stored.id;

        let orphan_dir = root.join("22222222-2222-2222-2222-222222222222");
        let mut orphan_stored = make_account(orphan_dir, "orphan@x.com", "org-orphan");
        orphan_stored.id = Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap();

        let outside = make_account(
            dir.path().join("elsewhere").join(".claude"),
            "outside@x.com",
            "org-outside",
        );

        let existing = vec![live_stored.clone(), orphan_stored.clone(), outside.clone()];
        let managed = vec![live_fresh];
        let reconciled = reconcile_stored_accounts(&existing, &managed, &root);

        let ids: Vec<_> = reconciled.iter().map(|a| a.id).collect();
        assert!(ids.contains(&live_stored.id));
        assert!(
            !ids.contains(&orphan_stored.id),
            "orphan under root dropped"
        );
        assert!(ids.contains(&outside.id), "row outside the root kept");
        let live = reconciled.iter().find(|a| a.id == live_stored.id).unwrap();
        assert_eq!(live.email_hint.as_deref(), Some("fresh@x.com"));

        clear_app_support_directory_override();
    }

    // [R10] a uuid-named dir keeps the same id before and after identity
    // hydration, and across consecutive discovery passes.
    #[test]
    fn discovery_id_is_stable_across_passes_for_uuid_named_dir() {
        let dir = tempfile::tempdir().unwrap();
        with_app_support_directory(dir.path().to_path_buf());
        ensure_directories().unwrap();
        let uuid = "f47ac10b-58cc-4372-a567-0e02b2c3d479";
        let managed = managed_configs_directory().join(uuid);
        write_credentials(&managed, "tok", "pro");
        let mut stored = make_account(managed.clone(), "e@x.com", "org-e");
        stored.id = Uuid::parse_str(uuid).unwrap();

        let mgr = ClaudeAccountManager::new();
        let pass1 = mgr
            .discover_managed_accounts(std::slice::from_ref(&stored))
            .unwrap();
        assert_eq!(pass1.len(), 1);
        let id1 = pass1[0].id;

        write_managed_claude_json(&managed, "e@x.com", "org-e");
        let pass2 = mgr
            .discover_managed_accounts(std::slice::from_ref(&stored))
            .unwrap();
        let pass3 = mgr.discover_managed_accounts(&pass2).unwrap();

        assert_eq!(id1, Uuid::parse_str(uuid).unwrap());
        assert_eq!(pass2[0].id, id1);
        assert_eq!(pass3[0].id, id1);

        clear_app_support_directory_override();
    }

    // [R11] switching back and forth between two managed dirs never regrows
    // `managed-configs/` and never adds a stored row.
    #[test]
    fn repeated_switching_between_two_managed_dirs_does_not_regrow() {
        let dir = tempfile::tempdir().unwrap();
        let rootp = dir.path();
        with_app_support_directory(rootp.to_path_buf());
        let ambient_dir = rootp.join(".claude");
        with_ambient_claude_config_dir(ambient_dir.clone());
        let ambient_json = rootp.join(".claude.json");
        with_ambient_claude_json_path(ambient_json.clone());
        ensure_directories().unwrap();

        let dir_a = managed_configs_directory().join("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa");
        write_credentials(&dir_a, "token-a", "pro");
        write_managed_claude_json(&dir_a, "a@x.com", "org-a");
        let dir_b = managed_configs_directory().join("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb");
        write_credentials(&dir_b, "token-b", "max");
        write_managed_claude_json(&dir_b, "b@x.com", "org-b");

        write_credentials(&ambient_dir, "token-a", "pro");
        write_ambient_claude_json(&ambient_json, "a@x.com", "org-a");

        let mut acct_a = make_account(dir_a.clone(), "a@x.com", "org-a");
        acct_a.id = Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap();
        let mut acct_b = make_account(dir_b.clone(), "b@x.com", "org-b");
        acct_b.id = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();
        let existing = vec![acct_a.clone(), acct_b.clone()];

        let mgr = ClaudeAccountManager::new();
        for target in [&acct_b, &acct_a, &acct_b, &acct_a] {
            mgr.switch_active_account(target, &existing).unwrap();
            let dir_count = fs::read_dir(managed_configs_directory())
                .unwrap()
                .filter(|e| e.as_ref().unwrap().path().is_dir())
                .count();
            assert_eq!(dir_count, 2, "managed-configs must stay at 2 dirs");
            let managed = mgr.discover_managed_accounts(&existing).unwrap();
            let reconciled =
                reconcile_stored_accounts(&existing, &managed, &managed_configs_directory());
            assert_eq!(reconciled.len(), 2, "reconciled list must stay at 2 rows");
        }

        clear_app_support_directory_override();
        clear_ambient_claude_config_dir_override();
        clear_ambient_claude_json_path_override();
    }
}
