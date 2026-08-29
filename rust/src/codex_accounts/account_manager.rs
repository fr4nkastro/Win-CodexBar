//! Codex account management: discovery, authentication, switching, removal.
//!
//! Port of `windows/.../account_manager.py` (MIT). Manages isolated managed
//! homes under `managed-homes/`, discovers the ambient `~/.codex` identity, and
//! switches the active identity by swapping `auth.json` into the ambient home,
//! rewriting the Codex Desktop `creator_id` global state and backing up/restoring
//! the desktop session.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use thiserror::Error;
use uuid::Uuid;

use super::api::{AuthBackedIdentity, CodexApiError, load_identity};
use super::file_locations::{
    ambient_codex_home, auth_backups_directory, codex_desktop_session_root,
    desktop_session_snapshot_path, ensure_directories, managed_homes_directory,
};
use super::login_runner::{CodexLoginOutcome, CodexLoginRunner, ManagedLoginProcess};
use super::models::{CodexAccount, CodexAccountSource, utc_now};

/// Friendly account manager error.
#[derive(Debug, Error)]
pub enum CodexAccountManagerError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<CodexApiError> for CodexAccountManagerError {
    fn from(value: CodexApiError) -> Self {
        CodexAccountManagerError::Message(value.to_string())
    }
}

/// Result of switching the active account.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSwitchResult {
    pub materialized_account: Option<CodexAccount>,
    pub backup_path: Option<PathBuf>,
    pub ambient_account: Option<CodexAccount>,
    pub desktop_session_backup_path: Option<PathBuf>,
    pub desktop_session_restore_path: Option<PathBuf>,
    pub desktop_session_restore_exists: bool,
}

/// Discovers, authenticates and switches Codex accounts.
#[derive(Debug, Default)]
pub struct CodexAccountManager;

impl CodexAccountManager {
    pub fn new() -> Self {
        Self
    }

    /// Start a `codex login` into a fresh managed home.
    pub fn add_managed_account(
        &self,
        handle: Option<&ManagedLoginProcess>,
    ) -> Result<CodexAccount, CodexAccountManagerError> {
        ensure_directories()?;
        let home_path = managed_homes_directory().join(Uuid::new_v4().to_string());
        fs::create_dir_all(&home_path)?;

        match self.authenticate_account(&home_path, CodexAccountSource::ManagedByApp, None, handle)
        {
            Ok(account) => Ok(account),
            Err(error) => {
                let _ = fs::remove_dir_all(&home_path);
                Err(error)
            }
        }
    }

    /// Re-run `codex login` for an existing account.
    pub fn reauthenticate(
        &self,
        account: &CodexAccount,
        handle: Option<&ManagedLoginProcess>,
    ) -> Result<CodexAccount, CodexAccountManagerError> {
        self.authenticate_account(
            &account.codex_home_path,
            account.source,
            Some(account),
            handle,
        )
    }

    /// Remove app-owned managed homes matching this account.
    pub fn remove_managed_files_if_owned(
        &self,
        account: &CodexAccount,
    ) -> Result<(), CodexAccountManagerError> {
        if !account.source.owns_files() {
            return Ok(());
        }

        let root = fs::canonicalize(managed_homes_directory())
            .unwrap_or_else(|_| managed_homes_directory());
        let targets = self.managed_home_paths_matching(account)?;

        for target in targets {
            let resolved = fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
            let relative = resolved.strip_prefix(&root).map_err(|_| {
                CodexAccountManagerError::Message(
                    "This path is not an app-managed home directory.".to_string(),
                )
            })?;
            if relative.as_os_str().is_empty() {
                return Err(CodexAccountManagerError::Message(
                    "Refusing to remove the managed-homes root.".to_string(),
                ));
            }
            if target.exists() {
                fs::remove_dir_all(&target)?;
            }
        }
        Ok(())
    }

    /// Discover managed homes and merge them against the stored accounts.
    pub fn discover_managed_accounts(
        &self,
        existing: &[CodexAccount],
    ) -> Result<Vec<CodexAccount>, CodexAccountManagerError> {
        ensure_directories()?;
        let mut discovered = Vec::new();
        let mut entries: Vec<PathBuf> = fs::read_dir(managed_homes_directory())?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_dir())
            .collect();
        entries.sort_by_key(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_lowercase())
                .unwrap_or_default()
        });
        for home_path in entries {
            if let Some(account) = self.discovered_managed_account(&home_path, existing) {
                discovered.push(account);
            }
        }
        Ok(discovered)
    }

    /// Discover the ambient `~/.codex` account.
    pub fn discover_ambient_account(&self, existing: &[CodexAccount]) -> Option<CodexAccount> {
        let home_path = ambient_codex_home();
        let auth_path = home_path.join("auth.json");
        if !home_path.is_dir() || !auth_path.exists() {
            return None;
        }
        let identity = load_identity(&home_path).ok()?;
        if identity.email.is_none() && identity.provider_account_id.is_none() {
            return None;
        }

        let candidate =
            candidate_account(identity.clone(), &home_path, CodexAccountSource::Ambient);
        let matched = existing.iter().find(|account| candidate.matches(account));
        let discovered_at = directory_timestamp(&home_path);
        Some(build_discovered_account(
            matched,
            identity,
            home_path,
            CodexAccountSource::Ambient,
            discovered_at,
        ))
    }

    /// Identity of the currently active (ambient) account, if any.
    pub fn load_active_identity(&self) -> Option<AuthBackedIdentity> {
        let auth_path = ambient_codex_home().join("auth.json");
        if !auth_path.exists() {
            return None;
        }
        load_identity(&ambient_codex_home()).ok()
    }

    /// Switch the ambient identity to `target`, materializing the previous
    /// ambient account as managed and preserving the desktop session.
    pub fn switch_active_account(
        &self,
        target: &CodexAccount,
        existing: &[CodexAccount],
    ) -> Result<CodexSwitchResult, CodexAccountManagerError> {
        ensure_directories()?;

        let target_auth_path = target.codex_home_path.join("auth.json");
        if !target_auth_path.exists() {
            return Err(CodexAccountManagerError::Message(
                "The selected account does not contain `auth.json`.".to_string(),
            ));
        }

        let ambient_account = self.discover_ambient_account(existing);
        let session_root = codex_desktop_session_root();
        let mut materialized_account: Option<CodexAccount> = None;
        if let Some(ambient) = &ambient_account {
            let is_ambient = ambient.source == CodexAccountSource::Ambient;
            if is_ambient && !ambient.matches(target) {
                materialized_account = Some(self.materialize_as_managed(ambient)?);
            }
        }

        let mut desktop_session_backup_path: Option<PathBuf> = None;
        let mut desktop_session_restore_path: Option<PathBuf> = None;
        let mut desktop_session_restore_exists = false;
        if session_root.is_some() {
            if let Some(materialized) = &materialized_account {
                desktop_session_backup_path =
                    Some(desktop_session_snapshot_path(&materialized.codex_home_path));
            }
            let snapshot_path = desktop_session_snapshot_path(&target.codex_home_path);
            desktop_session_restore_path = Some(snapshot_path.clone());
            desktop_session_restore_exists = path_has_children(&snapshot_path);
        }

        fs::create_dir_all(ambient_codex_home())?;
        let backup_path = self.backup_ambient_auth()?;
        fs::copy(&target_auth_path, ambient_codex_home().join("auth.json"))?;
        self.sync_ambient_global_state(
            ambient_account
                .as_ref()
                .and_then(|account| account.provider_account_id.clone()),
            self.target_account_id(target)?,
        );

        Ok(CodexSwitchResult {
            materialized_account,
            backup_path,
            ambient_account: self.discover_ambient_account(existing),
            desktop_session_backup_path,
            desktop_session_restore_path,
            desktop_session_restore_exists,
        })
    }

    /// Copy the ambient account into an app-managed home.
    pub fn materialize_as_managed(
        &self,
        account: &CodexAccount,
    ) -> Result<CodexAccount, CodexAccountManagerError> {
        ensure_directories()?;

        let source_auth_path = account.codex_home_path.join("auth.json");
        if !source_auth_path.exists() {
            return Err(CodexAccountManagerError::Message(
                "The current active account does not contain `auth.json`.".to_string(),
            ));
        }

        let destination_home = managed_homes_directory().join(Uuid::new_v4().to_string());
        fs::create_dir_all(&destination_home)?;
        fs::copy(&source_auth_path, destination_home.join("auth.json"))?;

        let now = utc_now();
        Ok(CodexAccount::new(
            account.id,
            account.nickname.clone(),
            account.email_hint.clone(),
            account.auth_subject.clone(),
            account.provider_account_id.clone(),
            destination_home,
            CodexAccountSource::ManagedByApp,
            account.created_at,
            now,
            Some(account.last_authenticated_at.unwrap_or(now)),
        ))
    }

    fn backup_ambient_auth(&self) -> Result<Option<PathBuf>, CodexAccountManagerError> {
        ensure_directories()?;
        let auth_path = ambient_codex_home().join("auth.json");
        if !auth_path.exists() {
            return Ok(None);
        }
        let backup_path =
            auth_backups_directory().join(format!("ambient-auth-{}.json", timestamp_slug()));
        fs::copy(&auth_path, &backup_path)?;
        Ok(Some(backup_path))
    }

    fn target_account_id(&self, target: &CodexAccount) -> Result<Option<String>, CodexApiError> {
        if let Some(account_id) = &target.provider_account_id {
            return Ok(Some(account_id.clone()));
        }
        let identity = load_identity(&target.codex_home_path)?;
        Ok(identity.provider_account_id)
    }

    fn sync_ambient_global_state(
        &self,
        previous_account_id: Option<String>,
        target_account_id: Option<String>,
    ) {
        let Some(target_account_id) = target_account_id else {
            return;
        };
        for file_name in [".codex-global-state.json", ".codex-global-state.json.bak"] {
            self.rewrite_creator_id(
                &ambient_codex_home().join(file_name),
                previous_account_id.as_deref(),
                &target_account_id,
            );
        }
    }

    fn rewrite_creator_id(
        &self,
        path: &Path,
        previous_account_id: Option<&str>,
        target_account_id: &str,
    ) {
        if !path.exists() {
            return;
        }
        let Ok(content) = fs::read_to_string(path) else {
            return;
        };
        let Ok(mut payload) = serde_json::from_str::<serde_json::Value>(&content) else {
            return;
        };
        if !payload.is_object() {
            return;
        }
        let Some(atom_state) = payload
            .get_mut("electron-persisted-atom-state")
            .and_then(|value| value.as_object_mut())
        else {
            return;
        };
        let Some(environment) = atom_state
            .get_mut("environment")
            .and_then(|value| value.as_object_mut())
        else {
            return;
        };
        let Some(creator_id) = environment.get("creator_id") else {
            return;
        };
        let Some(updated) =
            updated_creator_id(creator_id.as_str(), previous_account_id, target_account_id)
        else {
            return;
        };
        if updated == creator_id.as_str().unwrap_or_default() {
            return;
        }
        environment.insert("creator_id".to_string(), serde_json::Value::String(updated));
        let Ok(encoded) = serde_json::to_string_pretty(&payload) else {
            return;
        };
        let _ = fs::write(path, format!("{encoded}\n"));
    }

    fn managed_home_paths_matching(
        &self,
        account: &CodexAccount,
    ) -> Result<Vec<PathBuf>, CodexAccountManagerError> {
        ensure_directories()?;
        let mut targets: Vec<PathBuf> = vec![
            std::path::absolute(&account.codex_home_path)
                .unwrap_or_else(|_| account.codex_home_path.clone()),
        ];
        let mut seen_keys: std::collections::HashSet<String> =
            [managed_home_key(targets[0].as_path())]
                .into_iter()
                .collect();

        for entry in fs::read_dir(managed_homes_directory())? {
            let Ok(entry) = entry else {
                continue;
            };
            let home_path = entry.path();
            if !home_path.is_dir() {
                continue;
            }
            let Some(candidate) =
                self.discovered_managed_account(&home_path, std::slice::from_ref(account))
            else {
                continue;
            };
            if !candidate.matches(account) {
                continue;
            }
            let resolved = std::path::absolute(&home_path).unwrap_or_else(|_| home_path.clone());
            let key = managed_home_key(resolved.as_path());
            if seen_keys.contains(&key) {
                continue;
            }
            targets.push(resolved);
            seen_keys.insert(key);
        }
        Ok(targets)
    }

    fn authenticate_account(
        &self,
        home_path: &Path,
        source: CodexAccountSource,
        existing: Option<&CodexAccount>,
        handle: Option<&ManagedLoginProcess>,
    ) -> Result<CodexAccount, CodexAccountManagerError> {
        let result = CodexLoginRunner::run(home_path, Duration::from_secs(180), handle);

        match &result.outcome {
            CodexLoginOutcome::Cancelled => {
                return Err(CodexAccountManagerError::Message(
                    "Account setup cancelled.".to_string(),
                ));
            }
            CodexLoginOutcome::MissingBinary => {
                return Err(CodexAccountManagerError::Message(
                    "The `codex` command could not be found.".to_string(),
                ));
            }
            CodexLoginOutcome::TimedOut(_) => {
                return Err(CodexAccountManagerError::Message(
                    "The Codex sign-in flow timed out.".to_string(),
                ));
            }
            CodexLoginOutcome::LaunchFailed(output) => {
                return Err(CodexAccountManagerError::Message(format!(
                    "Failed to start the Codex sign-in flow: {output}"
                )));
            }
            CodexLoginOutcome::Failed(output) => {
                return Err(CodexAccountManagerError::Message(format!(
                    "The Codex sign-in flow did not complete.\n{output}"
                )));
            }
            CodexLoginOutcome::Success(_) => {}
        }

        let identity = load_identity(home_path)?;
        if identity.email.is_none() && identity.provider_account_id.is_none() {
            return Err(CodexAccountManagerError::Message(
                "Sign-in completed, but the account identity could not be read.".to_string(),
            ));
        }

        let now = utc_now();
        Ok(CodexAccount::new(
            existing
                .map(|account| account.id)
                .unwrap_or_else(Uuid::new_v4),
            existing.and_then(|account| account.nickname.clone()),
            identity
                .email
                .or_else(|| existing.and_then(|account| account.email_hint.clone())),
            identity
                .auth_subject
                .or_else(|| existing.and_then(|account| account.auth_subject.clone())),
            identity
                .provider_account_id
                .or_else(|| existing.and_then(|account| account.provider_account_id.clone())),
            home_path.to_path_buf(),
            source,
            existing.map(|account| account.created_at).unwrap_or(now),
            now,
            Some(now),
        ))
    }

    fn discovered_managed_account(
        &self,
        home_path: &Path,
        existing: &[CodexAccount],
    ) -> Option<CodexAccount> {
        if !home_path.is_dir() {
            return None;
        }
        let auth_path = home_path.join("auth.json");
        if !auth_path.exists() {
            return None;
        }
        let identity = load_identity(home_path).ok()?;
        if identity.email.is_none() && identity.provider_account_id.is_none() {
            return None;
        }

        let discovered_at = directory_timestamp(home_path);
        let candidate = candidate_account(
            identity.clone(),
            home_path,
            CodexAccountSource::ManagedByApp,
        );
        let matched = existing.iter().find(|account| candidate.matches(account));
        Some(build_discovered_account(
            matched,
            identity,
            home_path.to_path_buf(),
            CodexAccountSource::ManagedByApp,
            discovered_at,
        ))
    }
}

fn candidate_account(
    identity: AuthBackedIdentity,
    home_path: &Path,
    source: CodexAccountSource,
) -> CodexAccount {
    let id = stable_discovered_id(home_path, &identity);
    CodexAccount::new(
        id,
        None,
        identity.email.clone(),
        identity.auth_subject.clone(),
        identity.provider_account_id.clone(),
        home_path.to_path_buf(),
        source,
        utc_now(),
        utc_now(),
        None,
    )
}

/// Namespace for deterministically derived discovered-account ids.
///
/// Never change this string: changing it re-keys every derived id and would make
/// already-listed discovered accounts unresolvable on a later IPC call.
const DISCOVERED_ACCOUNT_ID_NAMESPACE: &str = "codexbar:codex-account:v1";

/// Deterministic id for an account discovered on disk.
///
/// Managed homes created by the app (and externally created ones) are named
/// `<uuid>`; that name IS the id. Otherwise the id is derived from the strongest
/// available stable identity key. Pure computation: no filesystem writes, no
/// network, no Windows-only syscalls, so it is fully exercised by `cargo test`
/// on Linux/WSL2.
fn stable_discovered_id(home_path: &Path, identity: &AuthBackedIdentity) -> Uuid {
    if let Some(id) = home_path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| Uuid::parse_str(name.trim()).ok())
    {
        return id;
    }
    derive_account_uuid(&stable_identity_key(home_path, identity))
}

/// First-available stable key: `provider_account_id` -> `auth_subject` -> home
/// path (equivalent to `CodexAccount::standardized_home_path()`).
fn stable_identity_key(home_path: &Path, identity: &AuthBackedIdentity) -> String {
    if let Some(value) = normalized(identity.provider_account_id.as_deref()) {
        return format!("provider:{value}");
    }
    if let Some(value) = normalized(identity.auth_subject.as_deref()) {
        return format!("subject:{value}");
    }
    format!("home:{}", managed_home_key(home_path))
}

/// Trim, drop-if-empty, lowercase — mirrors `models.rs::normalize_identifier` so
/// a derived id agrees with `CodexAccount::matches`.
fn normalized(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
}

/// Fold a stable key into a well-formed, deterministic `Uuid` via SHA-256.
///
/// The version nibble is set to `5` to denote the name-based family (not the
/// literal hash algorithm); the RFC 4122 variant bits are set so the value is a
/// round-trippable `Uuid`. Uses the existing `sha2` dependency — no new crate,
/// and the `uuid` `v5` feature is deliberately not enabled.
fn derive_account_uuid(key: &str) -> Uuid {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(DISCOVERED_ACCOUNT_ID_NAMESPACE.as_bytes());
    hasher.update([0x1f_u8]); // unit separator: removes namespace/key concat ambiguity
    hasher.update(key.as_bytes());
    let digest = hasher.finalize();

    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn build_discovered_account(
    matched: Option<&CodexAccount>,
    identity: AuthBackedIdentity,
    home_path: PathBuf,
    source: CodexAccountSource,
    discovered_at: DateTime<Utc>,
) -> CodexAccount {
    let id = matched
        .map(|account| account.id)
        .unwrap_or_else(|| stable_discovered_id(&home_path, &identity));
    CodexAccount::new(
        id,
        matched.and_then(|account| account.nickname.clone()),
        identity
            .email
            .or_else(|| matched.and_then(|account| account.email_hint.clone())),
        identity
            .auth_subject
            .or_else(|| matched.and_then(|account| account.auth_subject.clone())),
        identity
            .provider_account_id
            .or_else(|| matched.and_then(|account| account.provider_account_id.clone())),
        home_path,
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

fn directory_timestamp(path: &Path) -> DateTime<Utc> {
    let auth_path = path.join("auth.json");
    if auth_path.exists()
        && let Ok(metadata) = fs::metadata(&auth_path)
        && let Ok(modified) = metadata.modified()
    {
        return modified.into();
    }
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map(Into::into)
        .unwrap_or_else(|_| utc_now())
}

fn managed_home_key(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_lowercase()
}

fn path_has_children(path: &Path) -> bool {
    if !path.exists() || !path.is_dir() {
        return false;
    }
    fs::read_dir(path)
        .map(|mut entries| entries.next().is_some())
        .unwrap_or(false)
}

fn timestamp_slug() -> String {
    utc_now().format("%Y%m%d-%H%M%S").to_string()
}

/// Compute the replacement `creator_id` for the target account.
fn updated_creator_id(
    creator_id: Option<&str>,
    previous_account_id: Option<&str>,
    target_account_id: &str,
) -> Option<String> {
    let creator_id = creator_id?.trim();
    if creator_id.is_empty() {
        return None;
    }
    if creator_id == target_account_id || creator_id.ends_with(&format!("__{target_account_id}")) {
        return Some(creator_id.to_string());
    }
    if let Some(previous) = previous_account_id
        && creator_id.contains(previous)
    {
        return Some(creator_id.replace(previous, target_account_id));
    }
    if looks_like_uuid(creator_id) {
        return Some(target_account_id.to_string());
    }
    if let Some((prefix, suffix)) = creator_id.rsplit_once("__")
        && looks_like_uuid(suffix)
    {
        return Some(format!("{prefix}__{target_account_id}"));
    }
    None
}

fn looks_like_uuid(value: &str) -> bool {
    Uuid::parse_str(value.trim()).is_ok()
}

/// Id of the listed account whose identity matches the live `~/.codex`
/// identity, or `None` when nothing matches (or `identity` is `None`).
///
/// Pure computation: no filesystem, network, or Windows-only syscalls, so it is
/// fully exercised by `cargo test` on Linux/WSL2.
///
/// Mirrors [`CodexAccount::matches`] MINUS its home-path clause (post-switch the
/// managed home and `~/.codex` are different paths with identical auth, so path
/// matching would fail). Three ordered passes over `accounts` in slice order,
/// first hit wins:
/// 1. `provider_account_id` equal, present on both sides.
/// 2. `auth_subject` equal — skipping any account that also carries a
///    `provider_account_id` while the identity has one (they already failed
///    pass 1; this is the `models.rs` guard that stops a provider-id mismatch
///    from falling through to a subject/email match).
/// 3. `email` ↔ `email_hint` equal, same skip rule.
pub fn active_account_id(
    accounts: &[CodexAccount],
    identity: Option<&AuthBackedIdentity>,
) -> Option<Uuid> {
    let identity = identity?;
    let identity_provider = normalized(identity.provider_account_id.as_deref());
    let identity_subject = normalized(identity.auth_subject.as_deref());
    let identity_email = normalized(identity.email.as_deref());

    if let Some(identity_provider) = identity_provider.as_deref() {
        for account in accounts {
            if account.normalized_provider_account_id().as_deref() == Some(identity_provider) {
                return Some(account.id);
            }
        }
    }

    if let Some(identity_subject) = identity_subject.as_deref() {
        for account in accounts {
            if identity_provider.is_some() && account.normalized_provider_account_id().is_some() {
                continue;
            }
            if account.normalized_auth_subject().as_deref() == Some(identity_subject) {
                return Some(account.id);
            }
        }
    }

    if let Some(identity_email) = identity_email.as_deref() {
        for account in accounts {
            if identity_provider.is_some() && account.normalized_provider_account_id().is_some() {
                continue;
            }
            if account.normalized_email_hint().as_deref() == Some(identity_email) {
                return Some(account.id);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// Write an auth.json carrying a JWT identity for the given account id.
    fn write_auth(home_path: &Path, email: &str, account_id: &str) {
        let payload = serde_json::json!({
            "email": email,
            "sub": format!("auth0|{account_id}"),
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "team",
                "chatgpt_account_id": account_id,
            },
        });
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).unwrap());
        let auth_payload = serde_json::json!({
            "tokens": {
                "access_token": format!("access-{account_id}"),
                "refresh_token": format!("refresh-{account_id}"),
                "id_token": format!("header.{encoded}.signature"),
                "account_id": account_id,
            },
            "last_refresh": "2026-04-23T00:00:00Z",
        });
        std::fs::write(
            home_path.join("auth.json"),
            serde_json::to_vec_pretty(&auth_payload).unwrap(),
        )
        .unwrap();
    }

    fn make_account(home_path: PathBuf, email: &str, account_id: &str) -> CodexAccount {
        CodexAccount::new(
            Uuid::new_v4(),
            None,
            Some(email.to_string()),
            Some(format!("auth0|{account_id}")),
            Some(account_id.to_string()),
            home_path,
            CodexAccountSource::ManagedByApp,
            utc_now(),
            utc_now(),
            Some(utc_now()),
        )
    }

    /// Write an `auth.json` whose embedded JWT carries exactly `jwt_payload`,
    /// with an optional `tokens.account_id`. Lets a test control which identity
    /// fields are readable (provider id / subject / email).
    fn write_auth_payload(
        home_path: &Path,
        jwt_payload: serde_json::Value,
        token_account_id: Option<&str>,
    ) {
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&jwt_payload).unwrap());
        let mut tokens = serde_json::json!({
            "access_token": "access-token",
            "refresh_token": "refresh-token",
            "id_token": format!("header.{encoded}.signature"),
        });
        if let Some(account_id) = token_account_id {
            tokens["account_id"] = serde_json::Value::String(account_id.to_string());
        }
        let auth_payload = serde_json::json!({
            "tokens": tokens,
            "last_refresh": "2026-04-23T00:00:00Z",
        });
        std::fs::write(
            home_path.join("auth.json"),
            serde_json::to_vec_pretty(&auth_payload).unwrap(),
        )
        .unwrap();
    }

    fn identity_of(
        email: Option<&str>,
        auth_subject: Option<&str>,
        provider_account_id: Option<&str>,
    ) -> AuthBackedIdentity {
        AuthBackedIdentity {
            email: email.map(str::to_string),
            auth_subject: auth_subject.map(str::to_string),
            plan: None,
            provider_account_id: provider_account_id.map(str::to_string),
        }
    }

    fn account_fields(
        home_path: &str,
        email: Option<&str>,
        auth_subject: Option<&str>,
        provider_account_id: Option<&str>,
    ) -> CodexAccount {
        CodexAccount::new(
            Uuid::new_v4(),
            None,
            email.map(str::to_string),
            auth_subject.map(str::to_string),
            provider_account_id.map(str::to_string),
            PathBuf::from(home_path),
            CodexAccountSource::ManagedByApp,
            utc_now(),
            utc_now(),
            Some(utc_now()),
        )
    }

    #[test]
    fn active_account_id_matches_by_provider_account_id() {
        let a = account_fields("/h/a", Some("a@x.com"), Some("auth0|sub-a"), Some("prov-1"));
        let b = account_fields("/h/b", Some("b@x.com"), Some("auth0|sub-b"), Some("prov-2"));
        let accounts = vec![a, b.clone()];
        // Identity email/subject differ; only the normalized provider id agrees.
        let identity = identity_of(Some("other@x.com"), Some("auth0|other"), Some("PROV-2"));
        assert_eq!(
            active_account_id(&accounts, Some(&identity)),
            Some(b.id),
            "provider_account_id is the strongest key and is normalized"
        );
    }

    #[test]
    fn active_account_id_falls_back_to_auth_subject() {
        let a = account_fields("/h/a", Some("a@x.com"), Some("auth0|sub-a"), None);
        let b = account_fields("/h/b", Some("b@x.com"), Some("auth0|sub-b"), None);
        let accounts = vec![a, b.clone()];
        let identity = identity_of(Some("nomatch@x.com"), Some("AUTH0|SUB-B"), None);
        assert_eq!(active_account_id(&accounts, Some(&identity)), Some(b.id));
    }

    #[test]
    fn active_account_id_falls_back_to_email_hint() {
        let a = account_fields("/h/a", Some("a@x.com"), None, None);
        let b = account_fields("/h/b", Some("b@x.com"), None, None);
        let accounts = vec![a, b.clone()];
        let identity = identity_of(Some("B@X.COM"), None, None);
        assert_eq!(active_account_id(&accounts, Some(&identity)), Some(b.id));
    }

    #[test]
    fn active_account_id_provider_mismatch_blocks_subject_and_email_fallback() {
        // Account and identity share auth_subject and email, but both carry a
        // (different) provider_account_id -> models.rs:195-199 guard: no
        // fall-through to a subject/email match.
        let a = account_fields(
            "/h/a",
            Some("same@x.com"),
            Some("auth0|same-sub"),
            Some("prov-account"),
        );
        let identity = identity_of(
            Some("same@x.com"),
            Some("auth0|same-sub"),
            Some("prov-identity"),
        );
        assert_eq!(
            active_account_id(std::slice::from_ref(&a), Some(&identity)),
            None
        );
    }

    #[test]
    fn active_account_id_none_identity_is_none() {
        let a = account_fields("/h/a", Some("a@x.com"), Some("auth0|sub-a"), Some("prov-1"));
        assert_eq!(active_account_id(std::slice::from_ref(&a), None), None);
    }

    #[test]
    fn active_account_id_no_match_is_none() {
        let a = account_fields("/h/a", Some("a@x.com"), Some("auth0|sub-a"), Some("prov-1"));
        let identity = identity_of(Some("z@x.com"), Some("auth0|sub-z"), Some("prov-z"));
        assert_eq!(
            active_account_id(std::slice::from_ref(&a), Some(&identity)),
            None
        );
    }

    #[test]
    fn active_account_id_first_match_in_slice_order_wins() {
        let a = account_fields("/h/a", Some("dup@x.com"), None, None);
        let b = account_fields("/h/b", Some("dup@x.com"), None, None);
        let accounts = vec![a.clone(), b];
        let identity = identity_of(Some("dup@x.com"), None, None);
        assert_eq!(active_account_id(&accounts, Some(&identity)), Some(a.id));
    }

    #[test]
    fn active_account_id_picks_managed_account_matching_live_identity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        super::super::file_locations::with_app_support_directory(root.to_path_buf());

        let ambient_home = root.join(".codex");
        let managed_a = root.join("managed-homes").join("aaaaaaaa");
        let managed_b = root.join("managed-homes").join("bbbbbbbb");
        for home in [&ambient_home, &managed_a, &managed_b] {
            std::fs::create_dir_all(home).unwrap();
        }
        write_auth(&managed_a, "a@example.com", "provider-a");
        write_auth(&managed_b, "b@example.com", "provider-b");
        // Live ambient identity is the same account as managed_b.
        write_auth(&ambient_home, "b@example.com", "provider-b");
        super::super::file_locations::with_ambient_codex_home(ambient_home.clone());

        let manager = CodexAccountManager::new();
        let accounts = manager.discover_managed_accounts(&[]).unwrap();
        assert!(
            accounts
                .iter()
                .all(|account| account.source == CodexAccountSource::ManagedByApp),
            "every listed account is ManagedByApp (regression: source must not gate the active row)"
        );
        let identity = manager.load_active_identity();
        assert!(identity.is_some());

        let expected = accounts
            .iter()
            .find(|account| account.codex_home_path.as_path() == managed_b.as_path())
            .expect("managed_b discovered")
            .id;
        assert_eq!(
            active_account_id(&accounts, identity.as_ref()),
            Some(expected)
        );

        super::super::file_locations::clear_app_support_directory_override();
        super::super::file_locations::clear_ambient_codex_home_override();
    }

    #[test]
    fn remove_managed_account_removes_duplicate_homes_for_same_provider() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        super::super::file_locations::with_app_support_directory(root.to_path_buf());

        let account_id = "83c5ae92-f5ee-41f8-9528-199110d1d0f9";
        let first_home = root.join("managed-homes").join("first");
        let duplicate_home = root.join("managed-homes").join("duplicate");
        let other_home = root.join("managed-homes").join("other");
        for home in [&first_home, &duplicate_home, &other_home] {
            std::fs::create_dir_all(home).unwrap();
        }
        write_auth(&first_home, "user@example.com", account_id);
        write_auth(&duplicate_home, "user@example.com", account_id);
        write_auth(&other_home, "user@example.com", "different-provider");

        let account = make_account(first_home.clone(), "user@example.com", account_id);
        let manager = CodexAccountManager::new();
        manager.remove_managed_files_if_owned(&account).unwrap();

        assert!(!first_home.exists());
        assert!(!duplicate_home.exists());
        assert!(other_home.exists());

        super::super::file_locations::clear_app_support_directory_override();
    }

    #[test]
    fn switch_active_account_updates_global_state_creator_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        super::super::file_locations::with_app_support_directory(root.to_path_buf());

        let old_account_id = "1ea93d04-5c50-42e3-857b-3db850785967";
        let new_account_id = "83c5ae92-f5ee-41f8-9528-199110d1d0f9";

        let ambient_home = root.join(".codex");
        let target_home = root.join("managed-homes").join("target");
        let desktop_session_root = root.join("package-session");

        std::fs::create_dir_all(&ambient_home).unwrap();
        std::fs::create_dir_all(&target_home).unwrap();
        std::fs::create_dir_all(&desktop_session_root).unwrap();

        write_auth(&ambient_home, "old@example.com", old_account_id);
        write_auth(&target_home, "new@example.com", new_account_id);
        let target_session_dir = target_home.join("desktop-session").join("Network");
        std::fs::create_dir_all(&target_session_dir).unwrap();
        std::fs::write(target_session_dir.join("Cookies"), "cookie-data").unwrap();

        let global_state = serde_json::json!({
            "electron-persisted-atom-state": {
                "environment": {
                    "creator_id": format!("user-e9H3MsspGTF7UZJ8uaXuML55__{old_account_id}"),
                }
            }
        });
        for file_name in [".codex-global-state.json", ".codex-global-state.json.bak"] {
            std::fs::write(
                ambient_home.join(file_name),
                serde_json::to_vec_pretty(&global_state).unwrap(),
            )
            .unwrap();
        }

        super::super::file_locations::with_ambient_codex_home(ambient_home.clone());
        super::super::file_locations::with_codex_desktop_session_root(desktop_session_root.clone());

        let manager = CodexAccountManager::new();
        let target_account = make_account(target_home.clone(), "new@example.com", new_account_id);
        let result = manager
            .switch_active_account(&target_account, std::slice::from_ref(&target_account))
            .unwrap();

        let ambient_auth: serde_json::Value =
            serde_json::from_slice(&std::fs::read(ambient_home.join("auth.json")).unwrap())
                .unwrap();
        assert_eq!(ambient_auth["tokens"]["account_id"], new_account_id);
        assert_eq!(
            result
                .ambient_account
                .unwrap()
                .provider_account_id
                .as_deref(),
            Some(new_account_id)
        );
        let materialized = result.materialized_account.unwrap();
        assert_eq!(
            materialized.provider_account_id.as_deref(),
            Some(old_account_id)
        );
        assert_eq!(
            result.desktop_session_backup_path.unwrap(),
            materialized.codex_home_path.join("desktop-session")
        );
        assert_eq!(
            result.desktop_session_restore_path.unwrap(),
            target_home.join("desktop-session")
        );
        assert!(result.desktop_session_restore_exists);

        let backup_files: Vec<PathBuf> = std::fs::read_dir(root.join("auth-backups"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("ambient-auth-")
            })
            .collect();
        assert_eq!(backup_files.len(), 1);
        let backup: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&backup_files[0]).unwrap()).unwrap();
        assert_eq!(backup["tokens"]["account_id"], old_account_id);

        for file_name in [".codex-global-state.json", ".codex-global-state.json.bak"] {
            let payload: serde_json::Value =
                serde_json::from_slice(&std::fs::read(ambient_home.join(file_name)).unwrap())
                    .unwrap();
            let creator_id = payload["electron-persisted-atom-state"]["environment"]["creator_id"]
                .as_str()
                .unwrap();
            assert_eq!(
                creator_id,
                format!("user-e9H3MsspGTF7UZJ8uaXuML55__{new_account_id}")
            );
        }

        super::super::file_locations::clear_app_support_directory_override();
        super::super::file_locations::clear_ambient_codex_home_override();
        super::super::file_locations::clear_codex_desktop_session_root_override();
    }

    #[test]
    fn managed_home_named_uuid_uses_directory_name_as_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        super::super::file_locations::with_app_support_directory(root.to_path_buf());

        let uuid_name = "f47ac10b-58cc-4372-a567-0e02b2c3d479";
        let home = root.join("managed-homes").join(uuid_name);
        std::fs::create_dir_all(&home).unwrap();
        write_auth(&home, "user@example.com", "provider-xyz");

        let manager = CodexAccountManager::new();
        let discovered = manager.discover_managed_accounts(&[]).unwrap();
        let account = discovered
            .iter()
            .find(|a| a.codex_home_path.as_path() == home.as_path())
            .expect("managed home discovered");
        assert_eq!(account.id, Uuid::parse_str(uuid_name).unwrap());

        super::super::file_locations::clear_app_support_directory_override();
    }

    #[test]
    fn discovery_ids_are_stable_across_passes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        super::super::file_locations::with_app_support_directory(root.to_path_buf());

        let uuid_home = root
            .join("managed-homes")
            .join("f47ac10b-58cc-4372-a567-0e02b2c3d479");
        let plain_home = root.join("managed-homes").join("not-a-uuid-directory");
        let ambient_home = root.join(".codex");
        for home in [&uuid_home, &plain_home, &ambient_home] {
            std::fs::create_dir_all(home).unwrap();
        }
        write_auth(&uuid_home, "uuid@example.com", "provider-uuid-home");
        write_auth(&plain_home, "plain@example.com", "provider-plain-home");
        write_auth(
            &ambient_home,
            "ambient@example.com",
            "provider-ambient-home",
        );
        super::super::file_locations::with_ambient_codex_home(ambient_home.clone());

        let manager = CodexAccountManager::new();

        let pass_one = manager.discover_managed_accounts(&[]).unwrap();
        let pass_two = manager.discover_managed_accounts(&[]).unwrap();
        for home in [&uuid_home, &plain_home] {
            let first = pass_one
                .iter()
                .find(|a| a.codex_home_path.as_path() == home.as_path())
                .unwrap();
            let second = pass_two
                .iter()
                .find(|a| a.codex_home_path.as_path() == home.as_path())
                .unwrap();
            assert_eq!(first.id, second.id, "unstable id for {home:?}");
        }

        let ambient_one = manager.discover_ambient_account(&[]).unwrap();
        let ambient_two = manager.discover_ambient_account(&[]).unwrap();
        assert_eq!(ambient_one.id, ambient_two.id);

        super::super::file_locations::clear_app_support_directory_override();
        super::super::file_locations::clear_ambient_codex_home_override();
    }

    #[test]
    fn ambient_account_id_is_derived_and_stable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        super::super::file_locations::with_app_support_directory(root.to_path_buf());

        let ambient_home = root.join(".codex");
        std::fs::create_dir_all(&ambient_home).unwrap();
        write_auth(&ambient_home, "ambient@example.com", "acct-ambient-001");
        super::super::file_locations::with_ambient_codex_home(ambient_home.clone());

        let manager = CodexAccountManager::new();
        let first = manager.discover_ambient_account(&[]).unwrap();
        let second = manager.discover_ambient_account(&[]).unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(first.id, derive_account_uuid("provider:acct-ambient-001"));

        super::super::file_locations::clear_app_support_directory_override();
        super::super::file_locations::clear_ambient_codex_home_override();
    }

    #[test]
    fn managed_home_with_non_uuid_name_derives_stable_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        super::super::file_locations::with_app_support_directory(root.to_path_buf());

        let managed_root = root.join("managed-homes");
        let provider_home = managed_root.join("provider-keyed");
        let subject_home = managed_root.join("subject-keyed");
        let path_home = managed_root.join("path-keyed");
        for home in [&provider_home, &subject_home, &path_home] {
            std::fs::create_dir_all(home).unwrap();
        }

        // provider_account_id readable -> keyed by provider, ignoring subject.
        write_auth_payload(
            &provider_home,
            serde_json::json!({ "email": "p@example.com", "sub": "auth0|ignored-subject" }),
            Some("prov-1"),
        );
        // provider absent, auth_subject readable -> keyed by subject.
        write_auth_payload(
            &subject_home,
            serde_json::json!({ "email": "s@example.com", "sub": "auth0|subject-2" }),
            None,
        );
        // neither provider nor subject readable -> keyed by standardized home path.
        write_auth_payload(
            &path_home,
            serde_json::json!({ "email": "h@example.com" }),
            None,
        );

        let manager = CodexAccountManager::new();
        let pass_one = manager.discover_managed_accounts(&[]).unwrap();
        let pass_two = manager.discover_managed_accounts(&[]).unwrap();

        fn id_for(accounts: &[CodexAccount], home: &Path) -> Uuid {
            let found = accounts
                .iter()
                .find(|a| a.codex_home_path.as_path() == home);
            found.expect("home present in discovery").id
        }

        assert_eq!(
            id_for(&pass_one, provider_home.as_path()),
            derive_account_uuid("provider:prov-1")
        );
        assert_eq!(
            id_for(&pass_one, subject_home.as_path()),
            derive_account_uuid("subject:auth0|subject-2")
        );
        assert_eq!(
            id_for(&pass_one, path_home.as_path()),
            derive_account_uuid(&format!("home:{}", managed_home_key(&path_home)))
        );

        for home in [&provider_home, &subject_home, &path_home] {
            assert_eq!(
                id_for(&pass_one, home.as_path()),
                id_for(&pass_two, home.as_path())
            );
        }

        super::super::file_locations::clear_app_support_directory_override();
    }

    #[test]
    fn persisted_account_id_wins_over_derived_id() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        super::super::file_locations::with_app_support_directory(root.to_path_buf());

        let home = root.join("managed-homes").join("persisted-home");
        std::fs::create_dir_all(&home).unwrap();
        write_auth(&home, "stored@example.com", "provider-stored");

        let stored_id = Uuid::parse_str("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap();
        let mut stored = make_account(home.clone(), "stored@example.com", "provider-stored");
        stored.id = stored_id;

        let store = super::super::stores::AccountStore::new();
        store.save(std::slice::from_ref(&stored), None).unwrap();

        let manager = CodexAccountManager::new();
        let existing = store.load_accounts().unwrap();
        let discovered = manager.discover_managed_accounts(&existing).unwrap();
        let found = discovered
            .iter()
            .find(|a| a.codex_home_path.as_path() == home.as_path())
            .expect("home discovered");
        assert_eq!(found.id, stored_id, "discovery must reuse the persisted id");

        // Mimic the reconcile in load_codex_accounts: start from persisted, merge fresh.
        let mut reconciled = stored.clone();
        if let Some(fresh) = discovered.iter().find(|fresh| fresh.matches(&reconciled)) {
            reconciled.merge_from(fresh);
        }
        assert_eq!(reconciled.id, stored_id);

        store.save(std::slice::from_ref(&reconciled), None).unwrap();
        let reloaded = store.load_accounts().unwrap();
        assert_eq!(
            reloaded[0].id, stored_id,
            "accounts.json id must not be rewritten"
        );

        super::super::file_locations::clear_app_support_directory_override();
    }

    #[test]
    fn switch_to_discovered_account_resolved_by_id_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        super::super::file_locations::with_app_support_directory(root.to_path_buf());

        let ambient_home = root.join(".codex");
        let target_home = root.join("managed-homes").join("discovered-switch-target");
        let desktop_session_root = root.join("package-session");
        std::fs::create_dir_all(&ambient_home).unwrap();
        std::fs::create_dir_all(&target_home).unwrap();
        std::fs::create_dir_all(&desktop_session_root).unwrap();

        write_auth(
            &ambient_home,
            "old@example.com",
            "1ea93d04-5c50-42e3-857b-3db850785967",
        );
        write_auth(
            &target_home,
            "new@example.com",
            "83c5ae92-f5ee-41f8-9528-199110d1d0f9",
        );

        super::super::file_locations::with_ambient_codex_home(ambient_home.clone());
        super::super::file_locations::with_codex_desktop_session_root(desktop_session_root);

        let manager = CodexAccountManager::new();
        let mut accounts = manager.discover_managed_accounts(&[]).unwrap();
        if let Some(ambient) = manager.discover_ambient_account(&[]) {
            accounts.push(ambient);
        }

        let listed_id = accounts
            .iter()
            .find(|a| a.codex_home_path.as_path() == target_home.as_path())
            .expect("target discovered")
            .id
            .to_string();

        // Resolve exactly how commands/codex_accounts.rs does: by id string.
        let target = accounts
            .iter()
            .find(|a| a.id.to_string() == listed_id)
            .cloned()
            .expect("discovered account resolves by its listed id");

        manager
            .switch_active_account(&target, &accounts)
            .expect("switch by discovered id should succeed");

        super::super::file_locations::clear_app_support_directory_override();
        super::super::file_locations::clear_ambient_codex_home_override();
        super::super::file_locations::clear_codex_desktop_session_root_override();
    }
}
