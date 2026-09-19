//! Active Codex credential routing and per-auth-file fetch serialization.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};

use super::api::{CodexAccountApi, CodexApiError};
use super::credentials::{
    AuthCredentials, identity_from_credentials, load_identity, parse_credentials_json,
    write_auth_contents,
};
use super::models::{AccountUsageSnapshot, CodexAccountSource};

type CredentialLane = tokio::sync::Mutex<()>;
static CREDENTIAL_LANES: LazyLock<Mutex<HashMap<PathBuf, Weak<CredentialLane>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

struct FetchRoute {
    home: PathBuf,
    managed_copy: Option<PathBuf>,
}

fn credential_lane(home: &Path) -> Result<Arc<CredentialLane>, CodexApiError> {
    let path = home.join("auth.json").canonicalize().map_err(|error| {
        CodexApiError::Message(format!(
            "Could not resolve the account's auth file: {error}"
        ))
    })?;
    let mut lanes = CREDENTIAL_LANES
        .lock()
        .map_err(|error| CodexApiError::Message(error.to_string()))?;
    lanes.retain(|_, lane| lane.strong_count() > 0);
    if let Some(lane) = lanes.get(&path).and_then(Weak::upgrade) {
        return Ok(lane);
    }
    let lane = Arc::new(CredentialLane::new(()));
    lanes.insert(path, Arc::downgrade(&lane));
    Ok(lane)
}

fn resolve_fetch_route(codex_home_path: &Path) -> Result<FetchRoute, CodexApiError> {
    let target = super::account_manager::candidate_account(
        load_identity(codex_home_path)?,
        codex_home_path,
        CodexAccountSource::ManagedByApp,
    );
    let ambient = super::CodexAccountManager::new().discover_ambient_account(&[]);
    if let Some(ambient) = ambient.filter(|ambient| ambient.matches(&target)) {
        Ok(FetchRoute {
            home: ambient.codex_home_path,
            managed_copy: Some(codex_home_path.to_owned()),
        })
    } else {
        Ok(FetchRoute {
            home: codex_home_path.to_owned(),
            managed_copy: None,
        })
    }
}

fn synchronize_active_copy(ambient_home: &Path, managed_home: &Path) -> std::io::Result<()> {
    let ambient_path = ambient_home.join("auth.json").canonicalize()?;
    let managed_path = managed_home.join("auth.json").canonicalize()?;
    if ambient_path == managed_path {
        return Ok(());
    }
    let ambient_json = std::fs::read_to_string(ambient_path)?;
    let managed_json = std::fs::read_to_string(managed_path)?;
    if ambient_json == managed_json {
        return Ok(());
    }
    let ambient = parse_credentials_json(&ambient_json).map_err(std::io::Error::other)?;
    let managed = parse_credentials_json(&managed_json).map_err(std::io::Error::other)?;
    let account = |credentials: &AuthCredentials, home: &Path, source| {
        super::account_manager::candidate_account(
            identity_from_credentials(credentials),
            home,
            source,
        )
    };
    if !account(&ambient, ambient_home, CodexAccountSource::Ambient).matches(&account(
        &managed,
        managed_home,
        CodexAccountSource::ManagedByApp,
    )) {
        return Ok(());
    }
    if managed.last_refresh > ambient.last_refresh {
        write_auth_contents(ambient_home, managed_json.as_bytes())
    } else {
        write_auth_contents(managed_home, ambient_json.as_bytes())
    }
}

pub(super) async fn fetch_snapshot(
    api: &CodexAccountApi,
    codex_home_path: &Path,
    email_hint: Option<&str>,
    workspace_account_id: Option<&str>,
    verify_live_data: bool,
) -> Result<AccountUsageSnapshot, CodexApiError> {
    let _credentials = super::CREDENTIAL_OPERATIONS.read().await;
    let route = resolve_fetch_route(codex_home_path)?;
    fetch_home_snapshot(
        api,
        &route.home,
        email_hint,
        workspace_account_id,
        verify_live_data,
        route.managed_copy.as_deref(),
    )
    .await
}

pub(super) async fn fetch_home_snapshot(
    api: &CodexAccountApi,
    codex_home_path: &Path,
    email_hint: Option<&str>,
    workspace_account_id: Option<&str>,
    verify_live_data: bool,
    managed_copy: Option<&Path>,
) -> Result<AccountUsageSnapshot, CodexApiError> {
    let _home = credential_lane(codex_home_path)?.lock_owned().await;
    let synchronize = || {
        if let Some(managed) = managed_copy
            && let Err(error) = synchronize_active_copy(codex_home_path, managed)
        {
            tracing::warn!(
                "Could not synchronize the active Codex account's managed credentials: {error}"
            );
        }
    };
    synchronize();
    let result = api
        .fetch_locked_snapshot(
            codex_home_path,
            email_hint,
            workspace_account_id,
            verify_live_data,
        )
        .await;
    synchronize();
    result
}
