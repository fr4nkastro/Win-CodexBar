//! Domain model for Claude Code accounts and their usage snapshots.
//!
//! Shape deliberately mirrors `rust/src/codex_accounts/models.rs` so the two
//! account domains stay easy to compare, without sharing code (see design
//! decision D6: copy the pattern, do not extract a shared helper).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn utc_now() -> DateTime<Utc> {
    Utc::now()
}

fn normalize_identifier(value: Option<&str>) -> Option<String> {
    value
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty())
}

/// Where a Claude account's credentials live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ClaudeAccountSource {
    /// The environment's `CLAUDE_CONFIG_DIR` (or `~/.claude` when unset) — the
    /// identity the Claude CLI currently uses.
    Ambient,
    /// An app-owned config directory under `managed-configs/`.
    ManagedByApp,
}

impl ClaudeAccountSource {
    pub fn display_name(self) -> &'static str {
        match self {
            ClaudeAccountSource::Ambient => "System",
            ClaudeAccountSource::ManagedByApp => "Managed",
        }
    }

    /// Whether the app owns (and may delete) this account's files.
    pub fn owns_files(self) -> bool {
        matches!(self, ClaudeAccountSource::ManagedByApp)
    }

    pub fn from_raw(value: &str) -> Option<Self> {
        match value {
            "ambient" => Some(ClaudeAccountSource::Ambient),
            "managedByApp" => Some(ClaudeAccountSource::ManagedByApp),
            _ => None,
        }
    }
}

/// A stored Claude Code account.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeAccount {
    pub id: Uuid,
    pub nickname: Option<String>,
    pub email_hint: Option<String>,
    pub org_id: Option<String>,
    pub org_name: Option<String>,
    pub subscription_type: Option<String>,
    pub claude_config_dir: PathBuf,
    pub source: ClaudeAccountSource,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_authenticated_at: Option<DateTime<Utc>>,
}

impl ClaudeAccount {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Uuid,
        nickname: Option<String>,
        email_hint: Option<String>,
        org_id: Option<String>,
        org_name: Option<String>,
        subscription_type: Option<String>,
        claude_config_dir: PathBuf,
        source: ClaudeAccountSource,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
        last_authenticated_at: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            id,
            nickname,
            email_hint,
            org_id,
            org_name,
            subscription_type,
            claude_config_dir,
            source,
            created_at,
            updated_at,
            last_authenticated_at,
        }
    }

    pub fn display_name(&self) -> String {
        if let Some(nickname) = self
            .nickname
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return nickname.to_string();
        }
        if let Some(email) = self.email_hint.as_deref().filter(|s| !s.is_empty()) {
            return email.to_string();
        }
        self.claude_config_dir
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| self.claude_config_dir.display().to_string())
    }

    pub fn normalized_email_hint(&self) -> Option<String> {
        normalize_identifier(self.email_hint.as_deref())
    }

    pub fn normalized_org_id(&self) -> Option<String> {
        normalize_identifier(self.org_id.as_deref())
    }

    pub fn standardized_config_dir(&self) -> String {
        std::path::absolute(&self.claude_config_dir)
            .unwrap_or_else(|_| self.claude_config_dir.clone())
            .to_string_lossy()
            .to_lowercase()
    }

    fn source_priority(&self) -> u8 {
        if self.source.owns_files() { 2 } else { 1 }
    }

    fn recency_date(&self) -> DateTime<Utc> {
        self.last_authenticated_at.unwrap_or(self.updated_at)
    }

    /// Whether two accounts refer to the same identity.
    ///
    /// Mirrors `codex_accounts::models::CodexAccount::matches`: config-dir
    /// equality always matches; otherwise `org_id` is the strongest signal
    /// (and a mismatch on either side blocks a fallback to email so a
    /// provider-scoped account never gets conflated with an unrelated one
    /// sharing an email hint); `email_hint` is the last resort.
    pub fn matches(&self, other: &ClaudeAccount) -> bool {
        if self.standardized_config_dir() == other.standardized_config_dir() {
            return true;
        }
        if let (Some(a), Some(b)) = (self.normalized_org_id(), other.normalized_org_id())
            && a == b
        {
            return true;
        }
        if self.normalized_org_id().is_some() || other.normalized_org_id().is_some() {
            return false;
        }
        if let (Some(a), Some(b)) = (self.normalized_email_hint(), other.normalized_email_hint())
            && a == b
        {
            return true;
        }
        false
    }

    /// Merge a fresher discovery into this account, preferring managed/recency.
    pub fn merge_from(&mut self, other: &ClaudeAccount) {
        if self
            .nickname
            .as_deref()
            .map(str::trim)
            .is_none_or(|s| s.is_empty())
        {
            self.nickname = other.nickname.clone();
        }

        let prefer_other = other.source_priority() > self.source_priority()
            || (other.source_priority() == self.source_priority()
                && other.recency_date() >= self.recency_date());

        let pick = |mine: &mut Option<String>, value: Option<&String>| {
            let newer = prefer_other && value.is_some_and(|v| !v.trim().is_empty());
            if newer || mine.is_none() {
                *mine = value.cloned();
            }
        };
        pick(&mut self.email_hint, other.email_hint.as_ref());
        pick(&mut self.org_id, other.org_id.as_ref());
        pick(&mut self.org_name, other.org_name.as_ref());
        pick(
            &mut self.subscription_type,
            other.subscription_type.as_ref(),
        );

        if prefer_other {
            self.source = other.source;
            self.claude_config_dir = other.claude_config_dir.clone();
        }

        self.updated_at = self.updated_at.max(other.updated_at);
        self.last_authenticated_at = match (self.last_authenticated_at, other.last_authenticated_at)
        {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }
}

/// Identity of a previously-removed account, kept to avoid re-adding it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemovedAccountIdentity {
    pub id: Uuid,
    pub email_hint: Option<String>,
    pub org_id: Option<String>,
    pub claude_config_dir: PathBuf,
    pub source: ClaudeAccountSource,
    pub removed_at: DateTime<Utc>,
}

impl RemovedAccountIdentity {
    pub fn from_account(account: &ClaudeAccount) -> Self {
        Self {
            id: Uuid::new_v4(),
            email_hint: account.email_hint.clone(),
            org_id: account.org_id.clone(),
            claude_config_dir: account.claude_config_dir.clone(),
            source: account.source,
            removed_at: utc_now(),
        }
    }

    pub fn matches(&self, account: &ClaudeAccount) -> bool {
        if self.standardized_config_dir() == account.standardized_config_dir() {
            return true;
        }
        if let (Some(a), Some(b)) = (
            normalize_identifier(self.org_id.as_deref()),
            account.normalized_org_id(),
        ) && a == b
        {
            return true;
        }
        if self
            .org_id
            .as_ref()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
            || account.org_id.as_ref().is_some()
        {
            return false;
        }
        if let (Some(a), Some(b)) = (
            normalize_identifier(self.email_hint.as_deref()),
            account.normalized_email_hint(),
        ) && a == b
        {
            return true;
        }
        false
    }

    fn standardized_config_dir(&self) -> String {
        std::path::absolute(&self.claude_config_dir)
            .unwrap_or_else(|_| self.claude_config_dir.clone())
            .to_string_lossy()
            .to_lowercase()
    }
}

/// A single quota window (session or weekly).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindowSnapshot {
    pub used_percent: f64,
    pub reset_at: Option<DateTime<Utc>>,
    pub limit_window_seconds: i64,
}

impl UsageWindowSnapshot {
    pub fn new(
        used_percent: f64,
        reset_at: Option<DateTime<Utc>>,
        limit_window_seconds: i64,
    ) -> Self {
        Self {
            used_percent,
            reset_at,
            limit_window_seconds,
        }
    }
}

/// A fetched usage snapshot for one Claude account.
///
/// No credits field: unlike Codex, Claude's OAuth usage payload carries no
/// credits balance (design open question — confirmed no UI expects one).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeAccountUsageSnapshot {
    pub email: Option<String>,
    pub org_id: Option<String>,
    pub plan: Option<String>,
    pub primary_window: Option<UsageWindowSnapshot>,
    pub secondary_window: Option<UsageWindowSnapshot>,
    pub updated_at: DateTime<Utc>,
}

fn _path_is_trailing(path: &Path) -> bool {
    path.as_os_str()
        .to_string_lossy()
        .ends_with(std::path::MAIN_SEPARATOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(
        id: &str,
        dir: &str,
        source: ClaudeAccountSource,
        org_id: Option<&str>,
    ) -> ClaudeAccount {
        ClaudeAccount::new(
            Uuid::parse_str(id).unwrap(),
            None,
            None,
            org_id.map(str::to_string),
            None,
            None,
            PathBuf::from(dir),
            source,
            utc_now(),
            utc_now(),
            None,
        )
    }

    #[test]
    fn matches_by_config_dir() {
        let a = account(
            "11111111-1111-1111-1111-111111111111",
            "/x/a",
            ClaudeAccountSource::ManagedByApp,
            None,
        );
        let b = account(
            "22222222-2222-2222-2222-222222222222",
            "/x/a",
            ClaudeAccountSource::ManagedByApp,
            None,
        );
        assert!(a.matches(&b));
    }

    #[test]
    fn matches_by_org_id_case_insensitive() {
        let a = account(
            "11111111-1111-1111-1111-111111111111",
            "/x/a",
            ClaudeAccountSource::ManagedByApp,
            Some("org-1"),
        );
        let b = account(
            "22222222-2222-2222-2222-222222222222",
            "/y/b",
            ClaudeAccountSource::ManagedByApp,
            Some("ORG-1"),
        );
        assert!(a.matches(&b));
    }

    #[test]
    fn disambiguates_different_org_ids() {
        let a = account(
            "11111111-1111-1111-1111-111111111111",
            "/x/a",
            ClaudeAccountSource::ManagedByApp,
            Some("org-1"),
        );
        let b = account(
            "22222222-2222-2222-2222-222222222222",
            "/y/b",
            ClaudeAccountSource::ManagedByApp,
            Some("org-2"),
        );
        assert!(!a.matches(&b));
    }

    #[test]
    fn source_displays_and_ownership() {
        assert_eq!(ClaudeAccountSource::Ambient.display_name(), "System");
        assert_eq!(ClaudeAccountSource::ManagedByApp.display_name(), "Managed");
        assert!(ClaudeAccountSource::ManagedByApp.owns_files());
        assert!(!ClaudeAccountSource::Ambient.owns_files());
    }

    #[test]
    fn merge_prefers_managed_and_recency() {
        let mut managed = account(
            "11111111-1111-1111-1111-111111111111",
            "/x/managed",
            ClaudeAccountSource::ManagedByApp,
            None,
        );
        managed.nickname = Some("My acct".to_string());
        let ambient = account(
            "22222222-2222-2222-2222-222222222222",
            "~/.claude-like/ambient",
            ClaudeAccountSource::Ambient,
            None,
        );
        managed.merge_from(&ambient);
        assert_eq!(managed.source, ClaudeAccountSource::ManagedByApp);
        assert_eq!(managed.display_name(), "My acct");
    }

    #[test]
    fn display_name_falls_back_to_config_dir() {
        let acct = account(
            "11111111-1111-1111-1111-111111111111",
            "/x/my-config-dir",
            ClaudeAccountSource::ManagedByApp,
            None,
        );
        assert!(acct.display_name().contains("my-config-dir"));
        let _ = _path_is_trailing(std::path::Path::new("/x/"));
    }

    #[test]
    fn removed_identity_matches_by_org_id() {
        let acct = account(
            "11111111-1111-1111-1111-111111111111",
            "/x/a",
            ClaudeAccountSource::ManagedByApp,
            Some("org-9"),
        );
        let removed = RemovedAccountIdentity::from_account(&acct);
        assert!(removed.matches(&acct));

        let other = account(
            "22222222-2222-2222-2222-222222222222",
            "/y/b",
            ClaudeAccountSource::ManagedByApp,
            Some("org-other"),
        );
        assert!(!removed.matches(&other));
    }
}
